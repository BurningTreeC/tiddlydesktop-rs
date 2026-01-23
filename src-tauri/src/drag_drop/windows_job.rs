//! Windows Job Object for killing child processes when parent dies
//!
//! This module creates a Windows Job Object with JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE flag,
//! which ensures that all child processes (like Node.js TiddlyWiki servers) are automatically
//! terminated when the parent process exits, even if force-killed.

#![cfg(target_os = "windows")]

use std::ptr;
use std::sync::OnceLock;

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(
        lpJobAttributes: *mut std::ffi::c_void,
        lpName: *const u16,
    ) -> *mut std::ffi::c_void;
    fn SetInformationJobObject(
        hJob: *mut std::ffi::c_void,
        JobObjectInformationClass: u32,
        lpJobObjectInformation: *const std::ffi::c_void,
        cbJobObjectInformationLength: u32,
    ) -> i32;
    fn AssignProcessToJobObject(
        hJob: *mut std::ffi::c_void,
        hProcess: *mut std::ffi::c_void,
    ) -> i32;
    fn OpenProcess(
        dwDesiredAccess: u32,
        bInheritHandle: i32,
        dwProcessId: u32,
    ) -> *mut std::ffi::c_void;
    fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
}

const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
const JOBOBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
const PROCESS_ALL_ACCESS: u32 = 0x1F0FFF;

#[repr(C)]
struct JobObjectBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
struct JobObjectExtendedLimitInformation {
    basic_limit_information: JobObjectBasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

// Wrapper to make the handle Send+Sync (safe because Job Objects are thread-safe Windows handles)
struct JobHandle(*mut std::ffi::c_void);
unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

static JOB_HANDLE: OnceLock<JobHandle> = OnceLock::new();

/// Get or create the global job handle
pub fn get_job_handle() -> *mut std::ffi::c_void {
    JOB_HANDLE
        .get_or_init(|| {
            unsafe {
                let job = CreateJobObjectW(ptr::null_mut(), ptr::null());
                if job.is_null() {
                    return JobHandle(ptr::null_mut());
                }

                let mut info: JobObjectExtendedLimitInformation = std::mem::zeroed();
                info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                SetInformationJobObject(
                    job,
                    JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
                );

                JobHandle(job)
            }
        })
        .0
}

/// Assign a process to the job object so it gets killed when the parent exits
pub fn assign_process_to_job(pid: u32) {
    let job = get_job_handle();
    if job.is_null() {
        return;
    }

    unsafe {
        let process = OpenProcess(PROCESS_ALL_ACCESS, 0, pid);
        if !process.is_null() {
            AssignProcessToJobObject(job, process);
            CloseHandle(process);
        }
    }
}
