//! Local reproduction of the commit task's task-graph-output deserialization against the exact
//! payload persisted in `spider-db.jobs.serialized_job_outputs`.
//!
//! Usage: `cargo run --example commit_decode_probe -p compression-coordinator -- <payload.bin>`

use compression_coordinator::CompressionTaskOutput;
use spider_core::types::id::{JobId, ResourceGroupId, TaskId};
use spider_core::types::io::SerializedTaskOutputs;
use spider_tdl::TaskContext;

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "payload.bin".to_owned());
    let payload = std::fs::read(&path)?;
    println!(
        "payload: {} bytes, head={:02x?}",
        payload.len(),
        &payload[..payload.len().min(8)]
    );

    // (A) Direct decode, exactly like the commit task's `get_task_graph_outputs` internals.
    println!("\n== (A) direct SerializedTaskOutputs::deserialize_from_raw ==");
    match SerializedTaskOutputs::deserialize_from_raw(&payload) {
        Ok(outputs) => {
            println!("OK: {} output payloads", outputs.len());
            for (i, blob) in outputs.iter().enumerate() {
                let cto: CompressionTaskOutput = rmp_serde::from_slice(blob)?;
                println!(
                    "  [{i}] {} archive(s), dataset={:?}",
                    cto.archives.len(),
                    cto.dataset
                );
            }
        }
        Err(e) => println!("FAILED: {e}"),
    }

    // (B) Full TaskContext msgpack round-trip: exactly what the EM (`rmp_serde::to_vec`) and the
    // .so (`rmp_serde::from_slice` + `get_task_graph_outputs`) do across the FFI boundary.
    println!("\n== (B) TaskContext round-trip (EM to_vec -> .so from_slice -> get_task_graph_outputs) ==");
    let ctx = TaskContext::new(
        JobId::from(1),
        TaskId::Commit,
        1,
        ResourceGroupId::from(1),
        Some(payload),
    )
    .map_err(|e| anyhow::anyhow!("TaskContext::new failed: {e}"))?;
    let raw_ctx = rmp_serde::to_vec(&ctx)?;
    println!("raw_ctx (msgpack TaskContext): {} bytes", raw_ctx.len());
    let ctx2: TaskContext = rmp_serde::from_slice(&raw_ctx)?;
    match ctx2.get_task_graph_outputs() {
        Ok(Some(outs)) => println!("OK: {} outputs after round-trip", outs.len()),
        Ok(None) => println!("None (no outputs after round-trip)"),
        Err(e) => println!("FAILED after round-trip: {e}   <-- REPRODUCES THE CLUSTER BUG"),
    }
    Ok(())
}
