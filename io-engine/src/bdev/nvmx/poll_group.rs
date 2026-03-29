use std::{os::raw::c_void, ptr::NonNull};

use spdk_rs::libspdk::{
    spdk_fd_group, spdk_nvme_poll_group, spdk_nvme_poll_group_add,
    spdk_nvme_poll_group_create, spdk_nvme_poll_group_destroy,
    spdk_nvme_poll_group_get_fd_group, spdk_nvme_poll_group_remove,
    spdk_nvme_poll_group_set_interrupt_callback,
};

use crate::core::CoreError;

use super::QPair;

/// Wrapper for NVMe SPDK poll group structure.
pub(super) struct PollGroup(NonNull<spdk_nvme_poll_group>);

impl PollGroup {
    /// Creates a poll group.
    pub(super) fn create(ctx: *mut c_void, ctrlr_name: &str) -> Result<Self, CoreError> {
        let poll_group: *mut spdk_nvme_poll_group =
            unsafe { spdk_nvme_poll_group_create(ctx, std::ptr::null_mut()) };

        if poll_group.is_null() {
            Err(CoreError::GetIoChannel {
                name: ctrlr_name.to_string(),
            })
        } else {
            Ok(Self(NonNull::new(poll_group).unwrap()))
        }
    }

    /// Adds I/O qpair to poll group.
    pub(super) fn add_qpair(&mut self, qpair: &QPair) -> i32 {
        unsafe { spdk_nvme_poll_group_add(self.0.as_ptr(), qpair.as_ptr()) }
    }

    /// Removes I/O qpair to poll group.
    pub(super) fn remove_qpair(&mut self, qpair: &QPair) -> i32 {
        unsafe { spdk_nvme_poll_group_remove(self.0.as_ptr(), qpair.as_ptr()) }
    }

    /// Gets SPDK handle for poll group.
    #[inline(always)]
    pub(super) fn as_ptr(&self) -> *mut spdk_nvme_poll_group {
        self.0.as_ptr()
    }

    /// Returns the fd_group associated with this poll group (for interrupt
    /// mode fd_group nesting).
    pub(super) fn get_fd_group(&self) -> *mut spdk_fd_group {
        unsafe { spdk_nvme_poll_group_get_fd_group(self.0.as_ptr()) }
    }

    /// Registers the interrupt callback for non-completion events
    /// (e.g. qpair disconnection).
    pub(super) fn set_interrupt_callback(
        &self,
        cb_fn: Option<
            unsafe extern "C" fn(
                *mut spdk_nvme_poll_group,
                *mut c_void,
            ),
        >,
        cb_ctx: *mut c_void,
    ) -> i32 {
        unsafe {
            spdk_nvme_poll_group_set_interrupt_callback(
                self.0.as_ptr(),
                cb_fn,
                cb_ctx,
            )
        }
    }
}

impl Drop for PollGroup {
    fn drop(&mut self) {
        trace!("dropping poll group {:p}", self.0.as_ptr());
        let rc = unsafe { spdk_nvme_poll_group_destroy(self.0.as_ptr()) };
        if rc < 0 {
            error!("Error on poll group destroy: {}", rc);
        }
        trace!("poll group {:p} successfully dropped", self.0.as_ptr());
    }
}
