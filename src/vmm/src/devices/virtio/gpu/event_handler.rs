// Copyright 2024 - Firecracker GPU port
// Event handler for the virtio-gpu device.
//
// Follows the Firecracker pattern used by the virtio-net device:
//   - `init()` registers only the activate_evt while the device is inactive.
//   - On activate_evt, `process_activate_event` swaps to the runtime event
//     set (CTL and CUR queue eventfds).
//   - Queue events are forwarded to the worker thread via the Sender channel;
//     the worker owns all `VirtioGpu` / rutabaga state.

use event_manager::{EventOps, Events, MutEventSubscriber};
use log::{error, warn};
use vmm_sys_util::epoll::EventSet;

use crate::devices::virtio::device::VirtioDevice;

use super::device::Gpu;
use super::{CTL_INDEX, CUR_INDEX};

impl Gpu {
    // -----------------------------------------------------------------------
    // Event token constants
    // -----------------------------------------------------------------------

    const PROCESS_ACTIVATE: u32 = 0;
    const PROCESS_VIRTQ_CTL: u32 = 1;
    const PROCESS_VIRTQ_CUR: u32 = 2;

    // -----------------------------------------------------------------------
    // Event registration helpers
    // -----------------------------------------------------------------------

    fn register_activate_event(&self, ops: &mut EventOps) {
        if let Err(e) = ops.add(Events::with_data(
            &self.activate_evt,
            Self::PROCESS_ACTIVATE,
            EventSet::IN,
        )) {
            error!("virtio-gpu: failed to register activate_evt: {e}");
        }
    }

    fn register_runtime_events(&self, ops: &mut EventOps) {
        if let Err(e) = ops.add(Events::with_data(
            &self.queue_evts[CTL_INDEX],
            Self::PROCESS_VIRTQ_CTL,
            EventSet::IN,
        )) {
            error!("virtio-gpu: failed to register CTL queue event: {e}");
        }
        if let Err(e) = ops.add(Events::with_data(
            &self.queue_evts[CUR_INDEX],
            Self::PROCESS_VIRTQ_CUR,
            EventSet::IN,
        )) {
            error!("virtio-gpu: failed to register CUR queue event: {e}");
        }
    }

    // -----------------------------------------------------------------------
    // Event handlers
    // -----------------------------------------------------------------------

    fn process_activate_event(&self, ops: &mut EventOps) {
        // Drain the eventfd so it becomes quiescent again.
        if let Err(e) = self.activate_evt.read() {
            error!("virtio-gpu: failed to read activate_evt: {e:?}");
        }

        // Register the runtime queue events now that the queues are live.
        self.register_runtime_events(ops);

        // Unregister the one-shot activate event.
        if let Err(e) = ops.remove(Events::with_data(
            &self.activate_evt,
            Self::PROCESS_ACTIVATE,
            EventSet::IN,
        )) {
            error!("virtio-gpu: failed to unregister activate_evt: {e}");
        }
    }

    fn handle_ctl_event(&self) {
        if let Err(e) = self.queue_evts[CTL_INDEX].read() {
            error!("virtio-gpu: failed to read CTL queue event: {e:?}");
            return;
        }
        if let Some(sender) = &self.sender {
            if let Err(e) = sender.send(CTL_INDEX as u64) {
                error!("virtio-gpu: failed to send CTL notification to worker: {e:?}");
            }
        }
    }

    fn handle_cur_event(&self) {
        if let Err(e) = self.queue_evts[CUR_INDEX].read() {
            error!("virtio-gpu: failed to read CUR queue event: {e:?}");
            return;
        }
        // CUR (cursor) queue is not implemented for headless operation.
        // We drain the eventfd above so epoll does not spin, but send no
        // work to the worker.
    }
}

// ---------------------------------------------------------------------------
// MutEventSubscriber
// ---------------------------------------------------------------------------

impl MutEventSubscriber for Gpu {
    fn process(&mut self, event: Events, ops: &mut EventOps) {
        let source = event.data();
        let event_set = event.event_set();

        if !EventSet::IN.contains(event_set) {
            warn!("virtio-gpu: unexpected event set {event_set:?} from source {source:?}");
            return;
        }

        if self.is_activated() {
            match source {
                Self::PROCESS_ACTIVATE => self.process_activate_event(ops),
                Self::PROCESS_VIRTQ_CTL => self.handle_ctl_event(),
                Self::PROCESS_VIRTQ_CUR => self.handle_cur_event(),
                _ => warn!("virtio-gpu: spurious event from source {source:?}"),
            }
        } else {
            // Before activation only PROCESS_ACTIVATE should fire.
            match source {
                Self::PROCESS_ACTIVATE => self.process_activate_event(ops),
                _ => warn!(
                    "virtio-gpu: device not yet activated, spurious event \
                     from source {source:?}"
                ),
            }
        }
    }

    fn init(&mut self, ops: &mut EventOps) {
        // Called by the event manager when this subscriber is first added, and
        // again if the device is restored from a snapshot while already active.
        if self.is_activated() {
            self.register_runtime_events(ops);
        } else {
            self.register_activate_event(ops);
        }
    }
}
