// @author kongweiguang

use std::{cell::Cell, cell::RefCell, rc::Rc};

#[path = "../src/ime_composition.rs"]
mod ime_composition;

use ime_composition::{ImeCompositionHandler, same_composition_focus};

#[derive(Clone)]
struct BoundInput {
    binding: u64,
    active: Rc<Cell<bool>>,
    accepted: Rc<RefCell<Vec<u64>>>,
    rejected: Rc<RefCell<Vec<u64>>>,
    unmark_count: Rc<Cell<usize>>,
}

impl BoundInput {
    /// 模拟 pane 绑定处理器：旧 generation 失效后拒绝预编辑提交。
    fn commit(&mut self) {
        if self.active.get() {
            self.accepted.borrow_mut().push(self.binding);
        } else {
            self.rejected.borrow_mut().push(self.binding);
        }
    }

    /// 模拟 GPUI InputHandler 的 unmark 回调以核对取消和结束清理。
    fn unmark(&mut self) {
        self.unmark_count.set(self.unmark_count.get() + 1);
    }
}

/// 重复 START 不得把原 composition 重新绑定到后来获得焦点的 pane。
#[test]
fn repeated_start_preserves_the_original_binding() {
    let composition = ImeCompositionHandler::default();
    let accepted = Rc::new(RefCell::new(Vec::new()));
    let rejected = Rc::new(RefCell::new(Vec::new()));
    let original_active = Rc::new(Cell::new(true));
    let current = Cell::new(Some(BoundInput {
        binding: 7,
        active: original_active.clone(),
        accepted: accepted.clone(),
        rejected: rejected.clone(),
        unmark_count: Rc::new(Cell::new(0)),
    }));

    assert!(composition.begin(&current));
    original_active.set(false);
    assert_eq!(current.take().map(|handler| handler.binding), Some(7));
    current.set(Some(BoundInput {
        binding: 7,
        active: Rc::new(Cell::new(true)),
        accepted: accepted.clone(),
        rejected: rejected.clone(),
        unmark_count: Rc::new(Cell::new(0)),
    }));
    current.set(Some(BoundInput {
        binding: 8,
        active: Rc::new(Cell::new(true)),
        accepted: accepted.clone(),
        rejected: rejected.clone(),
        unmark_count: Rc::new(Cell::new(0)),
    }));
    assert!(!composition.begin(&current));
    assert!(composition.cancel(BoundInput::commit));

    assert!(accepted.borrow().is_empty());
    assert_eq!(&*rejected.borrow(), &[7]);
    assert_eq!(current.take().map(|handler| handler.binding), Some(8));
    composition.end(BoundInput::unmark);
}

/// 缺少 START 的组合消息不得借用新焦点处理器；无目标槽位也须保持锁定。
#[test]
fn composition_without_a_captured_handler_never_uses_the_current_one() {
    let composition = ImeCompositionHandler::<BoundInput>::default();
    let accepted = Rc::new(RefCell::new(Vec::new()));
    let rejected = Rc::new(RefCell::new(Vec::new()));
    let current = Cell::new(None);

    assert!(composition.begin(&current));
    current.set(Some(BoundInput {
        binding: 12,
        active: Rc::new(Cell::new(true)),
        accepted: accepted.clone(),
        rejected: rejected.clone(),
        unmark_count: Rc::new(Cell::new(0)),
    }));
    assert!(!composition.begin(&current));
    assert!(!composition.has_handler());
    assert!(!composition.cancel(BoundInput::commit));

    assert!(accepted.borrow().is_empty());
    assert_eq!(current.take().map(|handler| handler.binding), Some(12));
    composition.end(BoundInput::unmark);
}

/// 取消与 END 都清理固定目标的预编辑状态，END 后的新组合才能捕获新目标。
#[test]
fn cancel_and_end_clear_the_old_preedit_before_rebinding() {
    let composition = ImeCompositionHandler::default();
    let accepted = Rc::new(RefCell::new(Vec::new()));
    let rejected = Rc::new(RefCell::new(Vec::new()));
    let current = Cell::new(Some(BoundInput {
        binding: 21,
        active: Rc::new(Cell::new(true)),
        accepted: accepted.clone(),
        rejected: rejected.clone(),
        unmark_count: Rc::new(Cell::new(0)),
    }));

    assert!(composition.begin(&current));
    assert!(composition.cancel(BoundInput::unmark));
    assert!(!composition.begin(&current));
    assert_eq!(
        composition.with(|handler| handler.unmark_count.get()),
        Some(1)
    );
    composition.end(BoundInput::unmark);
    assert!(!composition.has_handler());

    current.set(Some(BoundInput {
        binding: 22,
        active: Rc::new(Cell::new(true)),
        accepted: accepted.clone(),
        rejected: rejected.clone(),
        unmark_count: Rc::new(Cell::new(0)),
    }));
    assert!(composition.begin(&current));
    assert_eq!(composition.with(|handler| handler.binding), Some(22));
    composition.end(BoundInput::unmark);
}

/// 组合 pin 与 GPUI 当前槽及缓存帧保留同一绑定，普通输入仍能使用原来的投递路径。
#[test]
fn composition_pin_keeps_current_and_cached_input_slots_usable() {
    let composition = ImeCompositionHandler::default();
    let accepted = Rc::new(RefCell::new(Vec::new()));
    let rejected = Rc::new(RefCell::new(Vec::new()));
    let current = Cell::new(Some(BoundInput {
        binding: 41,
        active: Rc::new(Cell::new(true)),
        accepted: accepted.clone(),
        rejected: rejected.clone(),
        unmark_count: Rc::new(Cell::new(0)),
    }));

    let cached_frame_handler = current.take().map(|handler| handler.clone());
    current.set(Some(BoundInput {
        binding: 41,
        active: Rc::new(Cell::new(true)),
        accepted: accepted.clone(),
        rejected: rejected.clone(),
        unmark_count: Rc::new(Cell::new(0)),
    }));
    assert!(composition.begin(&current));
    assert!(same_composition_focus(Some(41_u64), Some(41_u64)));

    let mut current_handler = current.take().expect("current platform slot remains set");
    current_handler.commit();
    current.set(Some(current_handler));
    cached_frame_handler
        .expect("cached frame retains its input handler")
        .commit();
    assert_eq!(composition.with(BoundInput::commit), Some(()));

    assert_eq!(&*accepted.borrow(), &[41, 41, 41]);
    assert!(rejected.borrow().is_empty());
    composition.end(BoundInput::unmark);
    assert_eq!(current.take().map(|handler| handler.binding), Some(41));
}

#[derive(Clone)]
struct DropProbe(Rc<Cell<usize>>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

/// 窗口销毁时由组合槽持有的处理器随其所有者释放，不留下旧 pane 引用。
#[test]
fn dropping_the_composition_slot_releases_its_handler() {
    let drops = Rc::new(Cell::new(0));
    let current = Cell::new(Some(DropProbe(drops.clone())));
    let composition = ImeCompositionHandler::default();
    assert!(composition.begin(&current));
    drop(composition);
    assert_eq!(drops.get(), 1);
    drop(current.take());
    assert_eq!(drops.get(), 2);
}

/// 回调中的嵌套 END 不得让已结束的旧 handler 在回调返回后重新进入组合槽。
#[test]
fn nested_end_does_not_restore_the_old_handler() {
    let composition = ImeCompositionHandler::default();
    let current = Cell::new(Some(BoundInput {
        binding: 31,
        active: Rc::new(Cell::new(true)),
        accepted: Rc::new(RefCell::new(Vec::new())),
        rejected: Rc::new(RefCell::new(Vec::new())),
        unmark_count: Rc::new(Cell::new(0)),
    }));

    assert!(composition.begin(&current));
    composition.with(|_| composition.end(BoundInput::unmark));

    assert!(!composition.has_handler());
    assert!(composition.begin(&current));
    assert_eq!(composition.with(|handler| handler.binding), Some(31));
}

/// 回调内 END 后启动新组合时，旧 epoch 结束不能覆盖新 handler。
#[test]
fn nested_end_then_start_preserves_the_new_epoch_handler() {
    let composition = ImeCompositionHandler::default();
    let accepted = Rc::new(RefCell::new(Vec::new()));
    let rejected = Rc::new(RefCell::new(Vec::new()));
    let current = Cell::new(Some(BoundInput {
        binding: 51,
        active: Rc::new(Cell::new(true)),
        accepted: accepted.clone(),
        rejected: rejected.clone(),
        unmark_count: Rc::new(Cell::new(0)),
    }));

    assert!(composition.begin(&current));
    composition.with(|old_handler| {
        assert_eq!(old_handler.binding, 51);
        composition.end(BoundInput::unmark);
        current.set(Some(BoundInput {
            binding: 52,
            active: Rc::new(Cell::new(true)),
            accepted: accepted.clone(),
            rejected: rejected.clone(),
            unmark_count: Rc::new(Cell::new(0)),
        }));
        assert!(composition.begin(&current));
    });

    assert_eq!(current.take().map(|handler| handler.binding), Some(52));
    assert!(composition.has_handler());
    assert_eq!(composition.with(|handler| handler.binding), Some(52));
    composition.end(BoundInput::unmark);
}

/// 回调内重复 START 属于同一组合 epoch，返回后仍保留原输入目标。
#[test]
fn nested_repeated_start_keeps_the_same_epoch_handler() {
    let composition = ImeCompositionHandler::default();
    let current = Cell::new(Some(BoundInput {
        binding: 61,
        active: Rc::new(Cell::new(true)),
        accepted: Rc::new(RefCell::new(Vec::new())),
        rejected: Rc::new(RefCell::new(Vec::new())),
        unmark_count: Rc::new(Cell::new(0)),
    }));

    assert!(composition.begin(&current));
    composition.with(|handler| {
        assert_eq!(handler.binding, 61);
        assert!(!composition.begin(&current));
    });

    assert!(composition.has_handler());
    assert_eq!(composition.with(|handler| handler.binding), Some(61));
    composition.end(BoundInput::unmark);
}

/// 新焦点或无文本目标会结束旧组合；旧绑定不能再写入，新焦点可立即建立新组合。
#[test]
fn focus_change_cancels_old_binding_and_allows_a_new_composition() {
    let composition = ImeCompositionHandler::default();
    let accepted = Rc::new(RefCell::new(Vec::new()));
    let rejected = Rc::new(RefCell::new(Vec::new()));
    let old_unmark_count = Rc::new(Cell::new(0));
    let current = Cell::new(Some(BoundInput {
        binding: 71,
        active: Rc::new(Cell::new(true)),
        accepted: accepted.clone(),
        rejected: rejected.clone(),
        unmark_count: old_unmark_count.clone(),
    }));

    assert!(composition.begin(&current));
    assert!(!same_composition_focus(Some(71_u64), Some(72_u64)));
    assert!(!same_composition_focus(Some(71_u64), None));
    assert!(!same_composition_focus(None::<u64>, None));
    composition.end(BoundInput::unmark);

    assert_eq!(old_unmark_count.get(), 1);
    assert!(!composition.is_active());
    assert!(composition.with(BoundInput::commit).is_none());
    assert!(accepted.borrow().is_empty());

    current.set(Some(BoundInput {
        binding: 72,
        active: Rc::new(Cell::new(true)),
        accepted: accepted.clone(),
        rejected,
        unmark_count: Rc::new(Cell::new(0)),
    }));
    assert!(composition.begin(&current));
    assert_eq!(composition.with(|handler| handler.binding), Some(72));
    assert_eq!(composition.with(BoundInput::commit), Some(()));
    assert_eq!(&*accepted.borrow(), &[72]);
    composition.end(BoundInput::unmark);
}
