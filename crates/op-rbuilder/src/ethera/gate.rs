use std::sync::Arc;

use super::XtPool;

/// Restores the XT pool gate if a candidate flashblock is abandoned after
/// executing an XT locally but before publication succeeds.
pub(crate) struct ExecutedXtGateGuard {
    xt_pool: Arc<XtPool>,
    instance_ids: Vec<String>,
    restore_on_drop: bool,
}

impl ExecutedXtGateGuard {
    pub(crate) fn new(xt_pool: Arc<XtPool>) -> Self {
        Self {
            xt_pool,
            instance_ids: Vec::new(),
            restore_on_drop: true,
        }
    }

    pub(crate) fn note_executed(&mut self, instance_id: String) {
        self.xt_pool.note_executed(&instance_id);
        self.instance_ids.push(instance_id);
    }

    pub(crate) fn disarm(mut self) -> Vec<String> {
        self.restore_on_drop = false;
        std::mem::take(&mut self.instance_ids)
    }
}

impl Drop for ExecutedXtGateGuard {
    fn drop(&mut self) {
        if self.restore_on_drop {
            self.xt_pool.restore_gate(&self.instance_ids);
        }
    }
}
