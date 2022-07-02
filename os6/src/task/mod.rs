//! Implementation of process management mechanism
//!
//! Here is the entry for process scheduling required by other modules
//! (such as syscall or clock interrupt).
//! By suspending or exiting the current process, you can
//! modify the process state, manage the process queue through TASK_MANAGER,
//! and switch the control flow through PROCESSOR.
//!
//! Be careful when you see [`__switch`]. Control flow around this function
//! might not be what you expect.

mod context;
mod manager;
mod pid;
mod processor;
mod switch;
#[allow(clippy::module_inception)]
mod task;

use alloc::sync::Arc;
use lazy_static::*;
use manager::fetch_task;
use switch::__switch;
use crate::mm::VirtAddr;
use crate::mm::MapPermission;
use crate::config::PAGE_SIZE;
use crate::timer::get_time_us;
pub use crate::syscall::process::TaskInfo;
use crate::fs::{open_file, OpenFlags};
pub use task::{TaskControlBlock, TaskStatus};

pub use context::TaskContext;
pub use manager::add_task;
pub use pid::{pid_alloc, KernelStack, PidHandle};
pub use processor::{
    current_task, current_trap_cx, current_user_token, run_tasks, schedule, take_current_task,
};

/// Make current task suspended and switch to the next task
pub fn suspend_current_and_run_next() {
    // There must be an application running.
    let task = take_current_task().unwrap();
    // ---- access current TCB exclusively
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    // Change status to Ready
    task_inner.task_stride += 10000000 / task_inner.task_priority;
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);
    // ---- release current PCB

    // push back to ready queue.
    add_task(task);
    // jump to scheduling cycle
    schedule(task_cx_ptr);
}

/// Exit current task, recycle process resources and switch to the next task
pub fn exit_current_and_run_next(exit_code: i32) {
    // take from Processor
    let task = take_current_task().unwrap();
    // **** access current TCB exclusively
    let mut inner = task.inner_exclusive_access();
    // Change status to Zombie
    inner.task_status = TaskStatus::Zombie;
    // Record exit code
    inner.exit_code = exit_code;
    // do not move to its parent but under initproc

    // ++++++ access initproc TCB exclusively
    {
        let mut initproc_inner = INITPROC.inner_exclusive_access();
        for child in inner.children.iter() {
            child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
            initproc_inner.children.push(child.clone());
        }
    }
    // ++++++ release parent PCB

    inner.children.clear();
    // deallocate user space
    inner.memory_set.recycle_data_pages();
    drop(inner);
    // **** release current PCB
    // drop task manually to maintain rc correctly
    drop(task);
    // we do not have to save task context
    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
}

lazy_static! {
    /// Creation of initial process
    ///
    /// the name "initproc" may be changed to any other app name like "usertests",
    /// but we have user_shell, so we don't need to change it.
    pub static ref INITPROC: Arc<TaskControlBlock> = Arc::new({
        let inode = open_file("ch6b_initproc", OpenFlags::RDONLY).unwrap();
        let v = inode.read_all();
        TaskControlBlock::new(v.as_slice())
    });
}

pub fn add_initproc() {
    add_task(INITPROC.clone());
}

pub fn memory_alloc(start: usize, len: usize, port: usize) -> isize {
    // println!("0x{:X} {}", start, len);
    if len == 0 {
        return 0;
    }
    if (len > 1073741824) || ((port & (!0x7)) != 0) || ((port & 0x7) == 0) || ((start % 4096) != 0) {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let mem_set = &mut inner.memory_set;
    let l: VirtAddr = start.into();
    let r: VirtAddr = (start + len).into();
    let lvpn = l.floor();
    let rvpn = r.ceil();
    // println!("L:{:?} R:{:?}", L, R);
    for area in &mem_set.areas {
        // println!("{:?} {:?}", area.vpn_range.l, area.vpn_range.r);
        if (lvpn <= area.vpn_range.get_start()) && (rvpn > area.vpn_range.get_start()) {
            return -1;
        }
    }
    let mut permission = MapPermission::from_bits((port as u8) << 1).unwrap();
    permission.set(MapPermission::U, true);
    // inner.tasks[current].memory_set.insert_framed_area(start.into(), (start + len).into(), permission);
    let mut start = start;
    let end = start + len;
    while start < end {
        let mut endr = start + PAGE_SIZE;
        if endr > end {
            endr = end;
        }
        mem_set.insert_framed_area(start.into(), endr.into(), permission);
        start = endr;
    }
    0
}

pub fn memory_free(start: usize, len: usize) -> isize {
    if len == 0 {
        return 0;
    }
    if start % 4096 != 0 {
        return -1;
    }
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let mem_set = &mut inner.memory_set;
    let l: VirtAddr = start.into();
    let r: VirtAddr = (start + len).into();
    let lvpn = l.floor();
    let rvpn = r.ceil();
    let mut cnt = 0;
    for area in &mem_set.areas {
        if (lvpn <= area.vpn_range.get_start()) && (rvpn > area.vpn_range.get_start()) {
            cnt += 1;
        }
    }
    if cnt < rvpn.0-lvpn.0 {
        return -1;
    }
    for i in 0..mem_set.areas.len() {
        if !mem_set.areas.get(i).is_some() {
            continue;
        }
        if (lvpn <= mem_set.areas[i].vpn_range.get_start()) && (rvpn > mem_set.areas[i].vpn_range.get_start()) {
            mem_set.areas[i].unmap(&mut mem_set.page_table);
            mem_set.areas.remove(i);
        }
    }
    0
}


pub fn get_task_info() -> TaskInfo {
    let task = current_task().unwrap();
    let inner = task.inner_exclusive_access();
    let new_info = TaskInfo {
        status: inner.task_status,
        syscall_times: inner.syscall_times,
        time: get_time_us() / 1000 - inner.start_time,
    };
    drop(inner);
    return new_info
}

pub fn set_task_priority(pri : usize) {
    let task = current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();
    task_inner.task_priority = pri;
    drop(task_inner);
}

