//! 侵入式双向链表 — RT-Thread `rtservice.h` 中 `rt_list` 的 Rust 移植
//!
//! 节点直接嵌入内核对象 (线程/定时器/IPC 对象) 内部, 零堆分配、
//! 零借用检查开销。全部操作须在临界区 (关中断) 内执行,
//! 与 RT-Thread 中 `rt_list_*` 的用法一致。
//!
//! # 不变量
//!
//! - 空链表: 头哨兵 `next == prev == null`;
//! - 首节点: `prev` 指向头哨兵 (移除首节点时据此维护 `head.next`);
//! - 尾节点: `next` 指向头哨兵 (哨兵结束标记, 遍历须以头哨兵
//!   指针为终止条件; 移除尾节点时据此维护 `head.prev`);
//! - 头哨兵本身 `linked` 恒为 false (头哨兵不视为已插入的节点)。

// 内核模块: unsafe 契约由临界区与模块文档统一说明, 函数体内不再逐段包裹
#![allow(unsafe_op_in_unsafe_fn)]

use core::ptr;

/// 双向链表节点
#[derive(Clone, Copy)]
pub(crate) struct ListHead {
    next: *mut ListHead,
    prev: *mut ListHead,
    /// 节点是否已链接在链表中 (O(1) 成员判定)
    linked: bool,
}

// 所有操作仅在临界区 (关中断) 内执行, 由内核保证同步
unsafe impl Sync for ListHead {}

/// 内核全局单元 — 为 `static` 提供 Sync 的 `UnsafeCell`
///
/// 内核全局状态 (就绪表/定时链表/僵尸队列等) 均经此类声明,
/// 访问须在临界区 (关中断) 内通过 [`KCell::get`] 进行。
pub(crate) struct KCell<T>(core::cell::UnsafeCell<T>);

// 内核保证所有访问均在临界区内进行
unsafe impl<T> Sync for KCell<T> {}

impl<T> KCell<T> {
    pub const fn new(value: T) -> Self {
        Self(core::cell::UnsafeCell::new(value))
    }

    /// 临界区内: 获取可变指针
    #[inline]
    pub unsafe fn get(&self) -> *mut T {
        self.0.get()
    }
}

impl ListHead {
    /// const 构造 (用于 `static` 初始化及内嵌于对象)
    pub const fn const_new() -> Self {
        Self {
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
            linked: false,
        }
    }

    /// 节点当前是否已链接在某个链表中
    #[inline]
    pub fn is_linked(&self) -> bool {
        self.linked
    }

    /// 下一个节点 (尾节点的 next 指向头哨兵)
    #[inline]
    pub fn next_node(&self) -> *mut ListHead {
        self.next
    }

    /// 将 `node` 插入到 `self` 之后 (self 为头哨兵时即队首插入)
    #[inline]
    pub unsafe fn insert_after(&mut self, node: *mut ListHead) {
        let n = unsafe { &mut *node };
        n.prev = self;
        if self.next.is_null() {
            // 空链表: 首节点 (next 指向头哨兵作结束标记)
            n.next = self;
            self.next = node;
            self.prev = node;
        } else {
            n.next = self.next; // 旧首节点
            unsafe { (*self.next).prev = node };
            self.next = node;
        }
        n.linked = true;
    }

    /// 将 `node` 追加到链表末尾 (self 为头哨兵)
    #[inline]
    pub unsafe fn push_back(&mut self, node: *mut ListHead) {
        let n = unsafe { &mut *node };
        n.next = self; // 队尾标记
        if self.prev.is_null() {
            // 空链表: 首节点
            n.prev = self;
            self.next = node;
            self.prev = node;
        } else {
            n.prev = self.prev; // 旧队尾
            unsafe { (*self.prev).next = node };
            self.prev = node;
        }
        n.linked = true;
    }

    /// 将 `node` 插入到 `self` 之前 (self 为常规节点, 须已链接)
    #[inline]
    pub unsafe fn insert_before(&mut self, node: *mut ListHead) {
        let n = unsafe { &mut *node };
        n.prev = self.prev;
        n.next = self;
        unsafe { (*self.prev).next = node };
        self.prev = node;
        n.linked = true;
    }

    /// 将节点从链表中移除 (未链接时无操作)
    ///
    /// 自动维护头哨兵的 next/prev: 移除首节点时更新 head.next,
    /// 移除尾节点时更新 head.prev (尾节点的 next 指向头哨兵)。
    #[inline]
    pub unsafe fn remove(&mut self) {
        if !self.linked {
            return;
        }
        if self.next == self.prev {
            // 唯一节点: 前后都指向头哨兵 → 清空链表
            unsafe { (*self.next).next = ptr::null_mut() };
            unsafe { (*self.next).prev = ptr::null_mut() };
        } else {
            unsafe { (*self.prev).next = self.next };
            unsafe { (*self.next).prev = self.prev };
        }
        self.next = ptr::null_mut();
        self.prev = ptr::null_mut();
        self.linked = false;
    }

    /// 链表是否为空 (头哨兵)
    #[inline]
    pub unsafe fn is_empty(&self) -> bool {
        self.next.is_null()
    }

    /// 第一个节点 (头哨兵)
    #[inline]
    pub unsafe fn first(&self) -> Option<*mut ListHead> {
        if self.next.is_null() {
            None
        } else {
            Some(self.next)
        }
    }

    /// 弹出第一个节点
    #[inline]
    pub unsafe fn pop_first(&mut self) -> Option<*mut ListHead> {
        let node = self.first()?;
        unsafe { (*node).remove() };
        Some(node)
    }
}
