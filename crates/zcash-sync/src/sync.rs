use anyhow::{anyhow, Result};
use futures::TryStreamExt;
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tonic::transport::Channel;
use zcash_client_backend::proto::{
    compact_formats::{
        CompactBlock, CompactOrchardAction as ProtoOrchardAction,
        CompactSaplingOutput as ProtoSaplingOutput,
    },
    service::{
        compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, TxFilter,
    },
};
use zcash_crypto::decrypt::{
    self, CompactOrchardAction, CompactSaplingOutput, CompactTransaction, DecryptedOutput,
    PreparedIvks,
};
use zcash_crypto::network::parse_network;
use zcash_keys::keys::UnifiedFullViewingKey;
use zcash_address::unified::{Encoding, Ufvk};
use zcash_protocol::consensus::Network;

use crate::client::{connect, UNARY_TIMEOUT};

/// Number of blocks being trial-decrypted concurrently in the streaming pipeline.
/// Set to half the available CPU count (min 2, max 16).
///
/// With N blocks in-flight simultaneously:
/// - Rayon workers are always fed with work even when individual blocks have
///   few transactions (which was limiting parallelism at heights > 1,700,000)
/// - The gRPC stream is consumed N× faster, reducing server-side deadline risk
/// - Backpressure is automatic: when all slots are busy, tokio stops polling
///   the stream → HTTP/2 flow control pauses the server organically
fn pipeline_depth() -> usize {
    std::thread::available_parallelism()
        .map(|p| (p.get() / 2).max(2).min(16))
        .unwrap_or(4)
}

// ─── public types ─────────────────────────────────────────────────────────────

/// Parameters for a shielded block range sync.
#[derive(Clone)]
pub struct SyncParams {
    /// gRPC endpoint URL (e.g. `"https://zaino-zec-testnet.nodes.stg.ledger-test.com/"`).
    pub grpc_url: String,
    /// Unified Full Viewing Key (UFVK) for the account to scan.
    pub viewing_key: String,
    /// First block height to scan (inclusive).
    pub start_height: u32,
    /// Last block height to scan (inclusive).
    pub end_height: u32,
    /// `"mainnet"` or `"testnet"` (defaults to `"testnet"` if `None`).
    pub network: Option<String>,
    /// Emit per-phase timing diagnostics to stderr every 10 seconds.
    pub verbose: bool,
    /// Called once for each block immediately after trial decryption completes.
    /// Intended for real-time progress indicators (e.g. `bar.inc(1)`).
    /// `None` disables the callback — always pass `None` from FFI bindings.
    pub on_block_done: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Called once for each matched and fully-decrypted transaction.
    /// Enables streaming results to the caller without waiting for the full scan.
    /// `None` disables the callback — results are only returned in `SyncResult`.
    pub on_transaction: Option<Arc<dyn Fn(ShieldedTransaction) + Send + Sync>>,
    /// When `true`, Sapling outputs are stripped from compact blocks before trial
    /// decryption. Only Orchard actions are trial-decrypted.
    ///
    /// This eliminates all Sapling cryptographic work. On dense post-NU5 blocks
    /// (height ≥ 1 687 104 on mainnet) this can reduce trial-decrypt time by up
    /// to 95% depending on the Sapling/Orchard output ratio.
    ///
    /// Set to `true` when the wallet only supports Orchard (e.g. Ledger).
    pub orchard_only: bool,
    /// Maximum number of retry attempts per range on transient errors (timeout,
    /// 503, etc.). On each retry the failing range is split in half, so up to
    /// `2^max_retries` sub-requests may be issued for a single original range.
    /// `None` or `Some(0)` disables retry entirely (single attempt).
    pub max_retries: Option<u32>,
}

/// A single shielded note found during decryption.
#[derive(Debug, Clone)]
pub struct ShieldedNote {
    /// Value in zatoshis.
    pub amount: u64,
    /// `"incoming"`, `"outgoing"`, or `"internal"`.
    pub transfer_type: String,
    /// Memo text (UTF-8, null-trimmed).
    pub memo: String,
}

/// A matched and fully-decrypted shielded transaction.
#[derive(Debug, Clone)]
pub struct ShieldedTransaction {
    /// Transaction ID in big-endian (display) hex order.
    pub txid: String,
    /// Raw transaction bytes as a hex string.
    pub hex: String,
    /// Block height at which this transaction was confirmed.
    pub block_height: u32,
    /// Block hash in big-endian (display) hex order.
    pub block_hash: String,
    /// Block timestamp (Unix seconds).
    pub block_time: u32,
    /// Transaction fee in zatoshis, computed via `TransactionData::fee_paid`.
    /// Zero for transactions with transparent inputs (prevout values unavailable from compact blocks).
    pub fee_zatoshis: i64,
    /// Decrypted Sapling notes belonging to this account.
    pub sapling_notes: Vec<ShieldedNote>,
    /// Decrypted Orchard notes belonging to this account.
    pub orchard_notes: Vec<ShieldedNote>,
}

/// Result returned after scanning a block range.
#[derive(Debug)]
pub struct SyncResult {
    pub transactions: Vec<ShieldedTransaction>,
    pub blocks_scanned: u32,
    pub elapsed_ms: u64,
    /// Total time (ms) spent waiting for the next block from the gRPC stream.
    pub stream_wait_ms: u64,
    /// Total time (ms) spent in trial decryption across all blocks.
    pub trial_decrypt_ms: u64,
    /// Total time (ms) spent on GetTransaction RPCs.
    pub get_transaction_ms: u64,
    /// Total time (ms) spent on full transaction decryption.
    pub full_decrypt_ms: u64,
}

// ─── public API ───────────────────────────────────────────────────────────────

/// Returns `true` if the error is transient and worth retrying with a smaller range.
///
/// Covers:
/// - gRPC deadline / stream timeout
/// - HTTP 503 from the load balancer (returned when the node is under heavy load,
///   manifests as a malformed gRPC frame because tonic receives an HTML error page)
fn is_retryable_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("timeout")
        || s.contains("deadline")
        || s.contains("timed out")
        || s.contains("503")
        || s.contains("service unavailable")
        || (s.contains("internal error") && s.contains("compression flag"))
}

/// Scan a range of compact blocks for shielded transactions belonging to the
/// UFVK in `params`.
///
/// When `params.max_retries` is set to a non-zero value, transient errors
/// (timeouts, 503s) trigger an automatic retry: the failing sub-range is split
/// in half and both halves are retried independently, up to `max_retries` times.
///
/// # Algorithm
///
/// 1. Parse the network and pre-compute IVKs from the UFVK (done once).
/// 2. Connect to the gRPC endpoint with TLS.
/// 3. Stream compact blocks via `GetBlockRange` (efficient single RPC).
/// 4. For each block: trial-decrypt compact outputs/actions to identify matching txids.
/// 5. For each matching txid: fetch the full transaction via `GetTransaction`.
/// 6. Full-decrypt the transaction to extract notes with memos and transfer types.
///
/// # Errors
///
/// Returns an error if the gRPC connection fails, UFVK is invalid, or the
/// block stream is interrupted (and all retries are exhausted).
pub async fn run_sync(params: SyncParams) -> Result<SyncResult> {
    let max_retries = params.max_retries.unwrap_or(0);
    if max_retries == 0 {
        return run_sync_inner(params).await;
    }

    let mut queue: VecDeque<(u32, u32, u32)> = VecDeque::new();
    queue.push_back((params.start_height, params.end_height, 0));

    let mut combined = SyncResult {
        transactions: Vec::new(),
        blocks_scanned: 0,
        elapsed_ms: 0,
        stream_wait_ms: 0,
        trial_decrypt_ms: 0,
        get_transaction_ms: 0,
        full_decrypt_ms: 0,
    };

    while let Some((s, e, attempts)) = queue.pop_front() {
        let mut sub_params = params.clone();
        sub_params.start_height = s;
        sub_params.end_height = e;
        sub_params.max_retries = None; // prevent re-entry

        match run_sync_inner(sub_params).await {
            Ok(result) => {
                combined.transactions.extend(result.transactions);
                combined.blocks_scanned += result.blocks_scanned;
                combined.elapsed_ms += result.elapsed_ms;
                combined.stream_wait_ms += result.stream_wait_ms;
                combined.trial_decrypt_ms += result.trial_decrypt_ms;
                combined.get_transaction_ms += result.get_transaction_ms;
                combined.full_decrypt_ms += result.full_decrypt_ms;
            }
            Err(ref err) if is_retryable_error(err) && attempts < max_retries => {
                let block_count = e - s + 1;
                let backoff_secs = 2u64.pow(attempts);
                if params.verbose {
                    eprintln!(
                        "[retry] {}..{} ({} blocks) timed out \
                         (attempt {}/{}) — waiting {}s then splitting in half",
                        s,
                        e,
                        block_count,
                        attempts + 1,
                        max_retries,
                        backoff_secs,
                    );
                } else {
                    eprintln!(
                        "  [retry] timeout on {}..{}, retrying as 2×{} blocks (attempt {}/{})",
                        s,
                        e,
                        block_count / 2,
                        attempts + 1,
                        max_retries,
                    );
                }
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;

                if block_count > 1 {
                    let mid = s + block_count / 2 - 1;
                    // Push halves at the front so they are processed before other pending ranges.
                    queue.push_front((mid + 1, e, attempts + 1));
                    queue.push_front((s, mid, attempts + 1));
                } else {
                    return Err(anyhow::anyhow!(
                        "chunk {}..{} timed out after {} retries and cannot be split further",
                        s,
                        e,
                        max_retries
                    ));
                }
            }
            Err(err) => return Err(err),
        }
    }

    Ok(combined)
}

/// Inner implementation — scans exactly one contiguous block range with no retry.
async fn run_sync_inner(params: SyncParams) -> Result<SyncResult> {
    let start = Instant::now();

    // 1. Resolve network and prepare IVKs + UFVK once (not per transaction).
    let (network, ivks, ufvk) =
        parse_sync_keys(params.network.as_deref(), &params.viewing_key)?;

    // 2. Connect to lightwalletd / Zaino with TLS.
    let channel = connect(&params.grpc_url).await?;
    let mut client: CompactTxStreamerClient<Channel> = CompactTxStreamerClient::new(channel);

    // 3. Stream compact blocks via GetBlockRange.
    let range = BlockRange {
        start: Some(BlockId { height: params.start_height as u64, hash: vec![] }),
        end: Some(BlockId { height: params.end_height as u64, hash: vec![] }),
    };
    let stream = client
        .get_block_range(range)
        .await
        .map_err(|e| anyhow!("GetBlockRange failed: {}", e))?
        .into_inner();

    // 4. Atomic counters shared between the pipeline futures and the diagnostic task.
    let trial_ms_atomic = Arc::new(AtomicU64::new(0));
    let blocks_atomic = Arc::new(AtomicU64::new(0));

    // 5. Background diagnostic task — emits a [diag] line to stderr every 10 s.
    //    Reads atomics non-blockingly; aborted once the pipeline finishes.
    let diag_handle = if params.verbose {
        Some(spawn_diagnostic_task(
            Arc::clone(&trial_ms_atomic),
            Arc::clone(&blocks_atomic),
        ))
    } else {
        None
    };

    // 6. Streaming pipeline — N blocks are trial-decrypted concurrently.
    //
    //    try_buffer_unordered(N) keeps N futures in-flight simultaneously.
    //    When all N slots are busy tokio stops polling the stream, triggering
    //    HTTP/2 flow control which naturally slows the server (backpressure).
    //    Results arrive in completion order, so we sort by height afterwards.

    // Keep a handle to read the final counter values after the pipeline finishes.
    let trial_ms_final = Arc::clone(&trial_ms_atomic);
    // Extract callbacks before moving params fields into closures.
    let on_block_done = params.on_block_done;
    let on_transaction = params.on_transaction;

    let mut block_results: Vec<TrialResult> = stream
        .map_err(|e| anyhow!("stream error: {}", e))
        .map_ok(move |block| {
            let ivks = Arc::clone(&ivks);
            let trial_ms_ref = Arc::clone(&trial_ms_atomic);
            let blocks_ref = Arc::clone(&blocks_atomic);
            let on_block_done = on_block_done.as_ref().map(Arc::clone);
            async move {
                process_compact_block(
                    block,
                    ivks,
                    params.orchard_only,
                    network,
                    trial_ms_ref,
                    blocks_ref,
                    on_block_done,
                )
                .await
            }
        })
        .try_buffer_unordered(pipeline_depth())
        .try_collect()
        .await?;

    // Pipeline finished — stop the diagnostic task.
    if let Some(h) = diag_handle {
        h.abort();
    }

    // Restore chronological order: try_buffer_unordered delivers results in
    // completion order (not stream order).
    block_results.sort_unstable_by_key(|r| r.height);

    // 7. Full-decrypt matched transactions (sequential; matching is rare).
    let mut all_transactions: Vec<ShieldedTransaction> = Vec::new();
    // Nullifiers of notes we received — used in Phase 4 to find spending txs.
    let mut our_nullifiers: std::collections::HashSet<[u8; 32]> = Default::default();
    let mut get_transaction_ms: u64 = 0;
    let mut full_decrypt_ms: u64 = 0;

    for result in &block_results {
        for txid_hex in &result.matched_txids {
            if let Some((tx, nullifiers)) = fetch_and_decrypt_tx(
                &mut client,
                txid_hex,
                result.height,
                &result.hash,
                result.time,
                &ufvk,
                network,
                &mut get_transaction_ms,
                &mut full_decrypt_ms,
            )
            .await?
            {
                // Collect Orchard nullifiers from incoming/internal notes.
                // These enable Phase 4: detecting txs that spend our notes (outgoing txs
                // that are invisible to trial decryption because they create no outputs for us).
                our_nullifiers.extend(nullifiers);
                if let Some(ref cb) = on_transaction {
                    cb(tx.clone());
                }
                all_transactions.push(tx);
            }
        }
    }

    // 8. Phase 4 — detect outgoing transactions via Orchard nullifier matching.
    //
    //    For each block in the scanned range, check whether any transaction spends
    //    a nullifier that corresponds to a note we received in Phase 2. Such a tx
    //    is an outgoing (spending) transaction that would not have been found by
    //    trial decryption alone, because trial decryption only identifies txs that
    //    create outputs *for us*.
    //
    //    We only run this pass when we actually received notes (our_nullifiers is
    //    non-empty) to avoid unnecessary work on scanning-only runs.
    if !our_nullifiers.is_empty() {
        let already_found: std::collections::HashSet<String> =
            all_transactions.iter().map(|tx| tx.txid.clone()).collect();

        for result in &block_results {
            for (txid, nfs) in &result.tx_nullifiers {
                if already_found.contains(txid) {
                    continue; // already processed in Phase 2 (e.g. self-send)
                }
                if !nfs.iter().any(|nf| our_nullifiers.contains(nf)) {
                    continue; // does not spend any of our notes
                }

                // This tx spends one of our received notes — fetch and full-decrypt it.
                if let Some((tx, _)) = fetch_and_decrypt_tx(
                    &mut client,
                    txid,
                    result.height,
                    &result.hash,
                    result.time,
                    &ufvk,
                    network,
                    &mut get_transaction_ms,
                    &mut full_decrypt_ms,
                )
                .await?
                {
                    if let Some(ref cb) = on_transaction {
                        cb(tx.clone());
                    }
                    all_transactions.push(tx);
                }
            }
        }

        // Re-sort after Phase 4 additions (spending txs may be at any height).
        all_transactions.sort_unstable_by_key(|tx| tx.block_height);
    }

    Ok(SyncResult {
        transactions: all_transactions,
        blocks_scanned: block_results.len() as u32,
        elapsed_ms: start.elapsed().as_millis() as u64,
        stream_wait_ms: 0, // not applicable in pipeline mode (stream consumed eagerly)
        trial_decrypt_ms: trial_ms_final.load(Ordering::Relaxed),
        get_transaction_ms,
        full_decrypt_ms,
    })
}

// ─── private helpers ──────────────────────────────────────────────────────────

/// Intermediate result produced by the trial-decrypt pipeline for one compact block.
struct TrialResult {
    height: u32,
    hash: String,
    time: u32,
    matched_txids: Vec<String>,
    /// Per-tx nullifiers: (txid, [nf_bytes, …]) for Phase 4 outgoing-tx detection.
    /// Each entry records the nullifiers spent by that transaction so we can match
    /// against nullifiers of notes we received in Phase 2.
    tx_nullifiers: Vec<(String, Vec<[u8; 32]>)>,
}

/// Parse the network string, prepare IVKs, and decode the UFVK.
/// These are computed once per `run_sync_inner` call, not per transaction.
fn parse_sync_keys(
    network_str: Option<&str>,
    viewing_key: &str,
) -> Result<(Network, Arc<PreparedIvks>, UnifiedFullViewingKey)> {
    let network = parse_network(network_str).map_err(|e| anyhow!("{}", e))?;
    let ivks = decrypt::prepare_ivks_arc(viewing_key).map_err(|e| anyhow!("{}", e))?;
    let (_net, ufvk_str) = Ufvk::decode(viewing_key)
        .map_err(|e| anyhow!("UFVK decode failed: {:?}", e))?;
    let ufvk = UnifiedFullViewingKey::parse(&ufvk_str)
        .map_err(|e| anyhow!("UFVK parse failed: {:?}", e))?;
    Ok((network, ivks, ufvk))
}

/// Spawn the background diagnostic task that emits a `[diag]` line every 10 s.
/// The returned handle must be aborted once the pipeline finishes.
fn spawn_diagnostic_task(
    trial_ms: Arc<AtomicU64>,
    blocks: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last_blocks: u64 = 0;
        let mut last_trial_ms: u64 = 0;
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        interval.tick().await; // skip the immediate first tick
        loop {
            interval.tick().await;
            let total_blocks = blocks.load(Ordering::Relaxed);
            let total_trial_ms = trial_ms.load(Ordering::Relaxed);
            let delta_blocks = total_blocks - last_blocks;
            let delta_trial_ms = total_trial_ms - last_trial_ms;
            let blps = delta_blocks as f64 / 10.0;
            eprintln!(
                "[diag] +{} blk in 10s ({:.0} bl/s)  trial={} ms  | total {} blk scanned",
                delta_blocks, blps, delta_trial_ms, total_blocks,
            );
            last_blocks = total_blocks;
            last_trial_ms = total_trial_ms;
        }
    })
}

/// Trial-decrypt one compact block and return the matched txids and per-tx nullifiers.
///
/// Offloads the CPU-bound decryption to Rayon via `spawn_blocking`. When
/// `orchard_only` is set and the block has no Orchard actions, the heavy path is
/// skipped entirely (fast path).
async fn process_compact_block(
    block: CompactBlock,
    ivks: Arc<PreparedIvks>,
    orchard_only: bool,
    network: Network,
    trial_ms_ref: Arc<AtomicU64>,
    blocks_ref: Arc<AtomicU64>,
    on_block_done: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<TrialResult> {
    let height = block.height as u32;
    let block_hash = hex::encode(block.hash.iter().copied().rev().collect::<Vec<u8>>());
    let block_time = block.time;

    // Fast path: when orchard_only is set, skip spawn_blocking entirely
    // for blocks where no transaction has any Orchard actions. This is an
    // O(1) check per block and eliminates all Rayon overhead for the
    // pre-NU5 era and for the vast majority of post-NU5 blocks that carry
    // only Sapling spam (sandblasting attack zone 1,687,104–2,100,000).
    if orchard_only && block.vtx.iter().all(|tx| tx.actions.is_empty()) {
        blocks_ref.fetch_add(1, Ordering::Relaxed);
        if let Some(ref cb) = on_block_done {
            cb();
        }
        return Ok(TrialResult {
            height,
            hash: block_hash,
            time: block_time,
            matched_txids: vec![],
            tx_nullifiers: vec![],
        });
    }

    let compact_txs: Vec<CompactTransaction> = block
        .vtx
        .iter()
        .map(|ctx| CompactTransaction {
            txid: hex::encode(ctx.hash.iter().copied().rev().collect::<Vec<u8>>()),
            sapling_outputs: if orchard_only {
                vec![]
            } else {
                ctx.outputs.iter().map(proto_sapling_to_compact).collect()
            },
            orchard_actions: ctx.actions.iter().map(proto_orchard_to_compact).collect(),
        })
        .collect();

    // Collect spent nullifiers per tx before moving compact_txs into
    // spawn_blocking. These are used in Phase 4 to detect outgoing txs
    // whose inputs spend notes we received in Phase 2.
    let tx_nullifiers: Vec<(String, Vec<[u8; 32]>)> = compact_txs
        .iter()
        .map(|tx| {
            let nfs: Vec<[u8; 32]> = tx
                .orchard_actions
                .iter()
                .filter_map(|a| a.nf.as_slice().try_into().ok())
                .collect();
            (tx.txid.clone(), nfs)
        })
        .collect();

    // Offload CPU-bound trial decryption to the blocking thread pool.
    // Rayon parallelises across outputs inside each block; the pipeline
    // parallelises across blocks.
    let t = Instant::now();
    let matched_txids = tokio::task::spawn_blocking(move || {
        decrypt::trial_decrypt_block(&compact_txs, &ivks, height, &network)
    })
    .await
    .map_err(|e| anyhow!("trial_decrypt_block panicked: {}", e))?;

    trial_ms_ref.fetch_add(t.elapsed().as_millis() as u64, Ordering::Relaxed);
    blocks_ref.fetch_add(1, Ordering::Relaxed);
    if let Some(ref cb) = on_block_done {
        cb();
    }

    Ok(TrialResult {
        height,
        hash: block_hash,
        time: block_time,
        matched_txids,
        tx_nullifiers,
    })
}

/// Fetch the full transaction bytes via `GetTransaction` and fully decrypt it.
///
/// Returns `Ok(None)` when decryption is skipped (e.g. pre-Overwinter format).
/// On success returns the decoded [`ShieldedTransaction`] and the Orchard nullifiers
/// of notes received by this account, so Phase 4 can detect spending transactions.
async fn fetch_and_decrypt_tx(
    client: &mut CompactTxStreamerClient<Channel>,
    txid_hex: &str,
    height: u32,
    block_hash: &str,
    block_time: u32,
    ufvk: &UnifiedFullViewingKey,
    network: Network,
    get_transaction_ms: &mut u64,
    full_decrypt_ms: &mut u64,
) -> Result<Option<(ShieldedTransaction, Vec<[u8; 32]>)>> {
    // GetTransaction — TxFilter.hash expects internal (little-endian) byte order.
    let txid_bytes_le: Vec<u8> = hex::decode(txid_hex)
        .map_err(|e| anyhow!("txid hex decode: {}", e))?
        .into_iter()
        .rev()
        .collect();

    let t_rpc = Instant::now();
    let mut req = tonic::Request::new(TxFilter {
        block: Some(BlockId { height: height as u64, hash: vec![] }),
        index: 0,
        hash: txid_bytes_le,
    });
    req.set_timeout(UNARY_TIMEOUT);
    let raw_tx = client
        .get_transaction(req)
        .await
        .map_err(|e| anyhow!("GetTransaction failed for {}: {}", txid_hex, e))?
        .into_inner();
    *get_transaction_ms += t_rpc.elapsed().as_millis() as u64;

    let tx_hex = hex::encode(&raw_tx.data);

    // Full decryption using pre-parsed UFVK (avoids re-parsing per transaction).
    // Pre-Overwinter transactions have an incompatible format; skip gracefully.
    let t_decrypt = Instant::now();
    let decrypted = match decrypt::full_decrypt_tx_with_ufvk(&tx_hex, ufvk, height, network) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "WARN: full_decrypt_tx skipped {} at height {}: {}",
                txid_hex, height, e
            );
            return Ok(None);
        }
    };
    *full_decrypt_ms += t_decrypt.elapsed().as_millis() as u64;

    // Collect received Orchard nullifiers before consuming decrypted.orchard_outputs.
    let received_nullifiers: Vec<[u8; 32]> = decrypted
        .orchard_outputs
        .iter()
        .filter_map(|note| note.nullifier)
        .collect();

    let tx = ShieldedTransaction {
        txid: txid_hex.to_string(),
        hex: tx_hex,
        block_height: height,
        block_hash: block_hash.to_string(),
        block_time,
        fee_zatoshis: decrypted.fee_zatoshis,
        sapling_notes: decrypted.sapling_outputs.into_iter().map(to_shielded_note).collect(),
        orchard_notes: decrypted.orchard_outputs.into_iter().map(to_shielded_note).collect(),
    };

    Ok(Some((tx, received_nullifiers)))
}

// ─── proto conversion helpers ─────────────────────────────────────────────────

/// Convert a proto `CompactSaplingOutput` to the zcash-crypto compact type.
fn proto_sapling_to_compact(p: &ProtoSaplingOutput) -> CompactSaplingOutput {
    CompactSaplingOutput {
        cmu: p.cmu.clone(),
        ephemeral_key: p.ephemeral_key.clone(),
        ciphertext: p.ciphertext.clone(),
    }
}

/// Convert a proto `CompactOrchardAction` to the zcash-crypto compact type.
fn proto_orchard_to_compact(p: &ProtoOrchardAction) -> CompactOrchardAction {
    CompactOrchardAction {
        nf: p.nullifier.clone(),
        cmx: p.cmx.clone(),
        ephemeral_key: p.ephemeral_key.clone(),
        ciphertext: p.ciphertext.clone(),
    }
}

/// Convert a [`DecryptedOutput`] from zcash-crypto to the gRPC layer's [`ShieldedNote`].
fn to_shielded_note(o: DecryptedOutput) -> ShieldedNote {
    ShieldedNote {
        amount: o.amount,
        transfer_type: o.transfer_type,
        memo: o.memo,
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// UFVK derived from the "abandon ×11 about" BIP-39 mnemonic on mainnet
    /// (account 0). This is a well-known test vector; no spending key material
    /// is involved.
    const TEST_UFVK: &str = "uview1qggz6nejagvka9wtm9r7xf84kkwy4cc0cgchptr98w0cyz33cj4958q5ulkd32nz2u3s0sp9yhcw7tu2n3nlw9x6ulghyd2zgc857tnzme2zpr3vn24zhtm2rjduv9a5zxlmzz404n7l0k69gmu4tfn2g3vpcn03rhz63e3l92fn8gra37tyly7utvgveswl20vz23pu84rc2nyqess38wvlgr2xzyhgj232ne5qutpe6ql6ghzetdy7pfzcmdzd5gd5dnwk25fwv7nnzmnty7u5ax3nzzgr6pdc905ckpd0s9v2cvn7e03qm7r46e5ngax536ywz7zxjptymm90px0rhvmqtwvttuy6d7degly023lqvskclk6mezyt69dwu6c4tfzrjgq4uuh5xa9m5dclgatykgtrrw268qe5pldfkx73f2kd5yyy2tjpjql92pa6tsk2nh2h88q23nee9z379het4akl6haqmuwf9d0nl0susg4tnxyk";

    fn test_params(grpc_url: &str) -> SyncParams {
        SyncParams {
            grpc_url: grpc_url.to_string(),
            viewing_key: TEST_UFVK.to_string(),
            start_height: 2_000_000,
            end_height: 2_000_010,
            network: Some("mainnet".to_string()),
            verbose: false,
            on_block_done: None,
            on_transaction: None,
            orchard_only: false,
            max_retries: None,
        }
    }

    // ── pipeline_depth ────────────────────────────────────────────────────────

    #[test]
    fn pipeline_depth_is_within_valid_range() {
        let depth = pipeline_depth();
        assert!(depth >= 2, "pipeline_depth must be at least 2, got {depth}");
        assert!(depth <= 16, "pipeline_depth must be at most 16, got {depth}");
    }

    // ── proto conversion helpers ──────────────────────────────────────────────

    #[test]
    fn proto_sapling_to_compact_preserves_all_fields() {
        let proto = ProtoSaplingOutput {
            cmu: vec![1u8; 32],
            ephemeral_key: vec![2u8; 32],
            ciphertext: vec![3u8; 52],
        };
        let compact = proto_sapling_to_compact(&proto);
        assert_eq!(compact.cmu, vec![1u8; 32]);
        assert_eq!(compact.ephemeral_key, vec![2u8; 32]);
        assert_eq!(compact.ciphertext, vec![3u8; 52]);
    }

    #[test]
    fn proto_orchard_to_compact_preserves_all_fields() {
        let proto = ProtoOrchardAction {
            nullifier: vec![1u8; 32],
            cmx: vec![2u8; 32],
            ephemeral_key: vec![3u8; 32],
            ciphertext: vec![4u8; 52],
        };
        let compact = proto_orchard_to_compact(&proto);
        assert_eq!(compact.nf, vec![1u8; 32]);
        assert_eq!(compact.cmx, vec![2u8; 32]);
        assert_eq!(compact.ephemeral_key, vec![3u8; 32]);
        assert_eq!(compact.ciphertext, vec![4u8; 52]);
    }

    #[test]
    fn to_shielded_note_conversion_preserves_fields() {
        let output = DecryptedOutput {
            amount: 42_000_000,
            memo: "test memo".to_string(),
            transfer_type: "incoming".to_string(),
            nullifier: None,
        };
        let note = to_shielded_note(output);
        assert_eq!(note.amount, 42_000_000);
        assert_eq!(note.memo, "test memo");
        assert_eq!(note.transfer_type, "incoming");
    }

    // ── run_sync early-fail paths (no network required) ───────────────────────

    /// `run_sync` must fail immediately with a clear error when the UFVK is
    /// invalid, before attempting any network connection.
    #[tokio::test]
    async fn run_sync_fails_immediately_with_invalid_ufvk() {
        let params = SyncParams {
            grpc_url: "https://127.0.0.1:1".to_string(), // unreachable — must not be reached
            viewing_key: "not_a_valid_ufvk".to_string(),
            start_height: 1_000_000,
            end_height: 1_000_010,
            network: Some("mainnet".to_string()),
            verbose: false,
            on_block_done: None,
            on_transaction: None,
            orchard_only: false,
            max_retries: None,
        };
        let err = run_sync(params).await.unwrap_err();
        // Must fail on UFVK parsing, not on connection
        assert!(
            !err.to_string().contains("gRPC connect failed"),
            "should fail before connecting, got: {err}"
        );
        assert!(
            err.to_string().to_lowercase().contains("ufvk")
                || err.to_string().to_lowercase().contains("decode"),
            "expected UFVK error, got: {err}"
        );
    }

    /// `run_sync` must fail immediately when the network string is invalid,
    /// before attempting any network connection.
    #[tokio::test]
    async fn run_sync_fails_immediately_with_invalid_network() {
        let params = SyncParams {
            grpc_url: "https://127.0.0.1:1".to_string(),
            viewing_key: TEST_UFVK.to_string(),
            start_height: 1_000_000,
            end_height: 1_000_010,
            network: Some("notanetwork".to_string()),
            verbose: false,
            on_block_done: None,
            on_transaction: None,
            orchard_only: false,
            max_retries: None,
        };
        let err = run_sync(params).await.unwrap_err();
        assert!(
            !err.to_string().contains("gRPC connect failed"),
            "should fail before connecting, got: {err}"
        );
    }

    /// `run_sync` must propagate a clear connection error when the port is
    /// closed (ECONNREFUSED), not hang or panic.
    #[tokio::test]
    async fn run_sync_propagates_connect_error_on_refused_port() {
        let addr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            drop(l);
            a
        };
        let url = format!("https://127.0.0.1:{}", addr.port());

        let err = run_sync(test_params(&url)).await.unwrap_err();
        assert!(
            err.to_string().contains("gRPC connect failed"),
            "unexpected error: {err}"
        );
    }

    /// `run_sync` must not hang indefinitely when a server accepts the TCP
    /// connection but never completes the TLS handshake (simulates a silent
    /// network drop or a non-TLS proxy intercepting the connection).
    ///
    /// The connect timeout must abort the attempt and return a clear error.
    #[tokio::test]
    async fn run_sync_connect_timeout_fires_when_server_stalls() {
        use crate::client::CONNECT_TIMEOUT;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Accept TCP connections but never send a TLS ServerHello.
        tokio::spawn(async move {
            loop {
                if let Ok((_sock, _)) = listener.accept().await {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
            }
        });

        tokio::time::pause();

        let url = format!("https://127.0.0.1:{port}");
        let sync_handle = tokio::spawn(run_sync(test_params(&url)));

        // Let the task start and register its connect timer.
        tokio::task::yield_now().await;
        // Advance past CONNECT_TIMEOUT so the timer fires without real waiting.
        tokio::time::advance(CONNECT_TIMEOUT + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        let result = sync_handle.await.unwrap();
        assert!(result.is_err(), "expected an error, got Ok");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("gRPC connect failed") || msg.contains("timeout") || msg.contains("transport"),
            "unexpected error: {msg}"
        );
    }
}
