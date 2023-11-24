//! Different servers that the clients can spawn and manage

use futures::Future;
use parking_lot::Mutex;
use tokio::task::AbortHandle;

pub mod blaze;
pub mod http;
pub mod redirector;
pub mod telemetry;

pub const REDIRECTOR_PORT: u16 = 42127;
pub const BLAZE_PORT: u16 = 42128;
pub const TELEMETRY_PORT: u16 = 42129;
pub const QOS_PORT: u16 = 42130;
pub const HTTP_PORT: u16 = 42131;

// Shared set of abort handles to server tasks
static SERVER_TASK_COLLECTION: Mutex<Vec<AbortHandle>> = Mutex::new(Vec::new());

/// Spawns a server related task future onto tokios runtime and
/// adds the abort handle for the task to the task collection
///
/// ## Arguments
/// * task - The task future to spawn
#[inline]
pub fn spawn_server_task<F>(task: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let handle = tokio::spawn(task);
    add_server_task(handle.abort_handle());
}

/// Append an abort handle to the server task collection
///
/// ## Arguments
/// * handle - The abort handle for the task
pub fn add_server_task(handle: AbortHandle) {
    let values = &mut *SERVER_TASK_COLLECTION.lock();
    values.push(handle);
}

/// Calls the abort handles for all tasks present
/// in the task collection
pub fn stop_server_tasks() {
    // Take the current tasks list from the task collection
    let values = {
        let values = &mut *SERVER_TASK_COLLECTION.lock();
        values.split_off(0)
    };

    // Call abort on each of the handles
    values.into_iter().for_each(|value| value.abort());
}
