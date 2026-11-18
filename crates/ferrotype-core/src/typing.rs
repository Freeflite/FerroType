//! The typing loop: a countdown, then one keystroke at a time with a random pause after each.
//!
//! It keeps typing when another window gets the focus (unless asked not to): over Remote Desktop
//! the window Windows reports often isn't the one really getting the keys. It only waits while
//! FerroType's own window has the focus, so it never types into itself and its Stop button is
//! always safe to click, and while Ctrl, Alt, Shift or Win is held, so keystrokes never turn into
//! shortcuts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::rng::Rng;

/// How keys actually get pressed. `ferrotype-sys` implements it with `SendInput`; the tests use
/// a fake.
pub trait Keyboard {
    /// A top-level window.
    type Window: Copy + PartialEq;
    /// A keyboard layout.
    type Layout: Copy;

    /// The window with the keyboard focus, if Windows will say.
    fn foreground(&self) -> Option<Self::Window>;
    /// Whether the window belongs to FerroType itself.
    fn is_own(&self, window: Self::Window) -> bool;
    /// The keyboard layout to type with (the target window's, if there is one).
    fn layout(&self, window: Option<Self::Window>) -> Self::Layout;
    /// Whether Ctrl, Alt, Shift or Win is held down.
    fn modifiers_held(&self) -> bool;
    /// Types one character, holding the key down for `hold`. An error means Windows turned the
    /// keystroke away (the session is locked, say); the character wasn't typed.
    fn type_char(&self, c: char, layout: Self::Layout, hold: Duration) -> Result<(), Refused>;
}

/// Windows turned a keystroke away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused;

/// Asks a typing job to stop; cheap to clone and share with the thread doing the typing.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Waits for `time`, or less if cancelled meanwhile; returns whether it was cancelled.
    /// Sleeps in short slices so both short key delays and a Stop are honoured promptly.
    pub fn sleep(&self, time: Duration) -> bool {
        let end = Instant::now() + time;
        loop {
            if self.is_cancelled() {
                return true;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            std::thread::sleep(left.min(Duration::from_millis(10)));
        }
    }
}

/// What to type and how.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Already prepared (see [`crate::text::prepare`]).
    pub text: String,
    /// Pause after each key, picked at random between these.
    pub min_delay: Duration,
    pub max_delay: Duration,
    /// Seconds before the first key, to click into the target window.
    pub countdown: u32,
    pub stop_on_focus_change: bool,
}

/// How a job is getting on, for the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Seconds left before typing starts.
    Countdown(u32),
    /// The countdown is over but FerroType still has the focus: waiting for a click elsewhere.
    WaitingForTarget,
    /// FerroType got the focus back partway; waiting for the target to get it again.
    PausedOwnWindow,
    PausedModifiers,
    /// Windows is turning keystrokes away; retrying.
    PausedRefused,
    /// `left` is an estimate, once there's enough to go on.
    Typing {
        done: usize,
        total: usize,
        left: Option<Duration>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done {
        typed: usize,
    },
    Stopped {
        typed: usize,
    },
    /// Another window got the focus (only with `stop_on_focus_change`).
    FocusMoved {
        typed: usize,
    },
    /// Stopped before anything was typed.
    Cancelled,
}

/// Runs a typing job on the calling thread, reporting progress as it goes.
pub fn run<K: Keyboard>(
    keyboard: &K,
    plan: &Plan,
    cancel: &Cancel,
    rng: &mut Rng,
    mut report: impl FnMut(Progress),
) -> Outcome {
    let chars: Vec<char> = plan.text.chars().collect();
    let total = chars.len();
    for remaining in (1..=plan.countdown).rev() {
        report(Progress::Countdown(remaining));
        if cancel.sleep(Duration::from_secs(1)) {
            return Outcome::Cancelled;
        }
    }
    let stopped = |typed: usize| if typed == 0 { Outcome::Cancelled } else { Outcome::Stopped { typed } };
    let mut target = None;
    let mut layout = None;
    let mut started = Instant::now();
    let mut last_report: Option<Instant> = None;

    for (i, &c) in chars.iter().enumerate() {
        if keyboard.modifiers_held() {
            report(Progress::PausedModifiers);
            while keyboard.modifiers_held() {
                if cancel.sleep(Duration::from_millis(50)) {
                    return stopped(i);
                }
            }
            last_report = None;
        }
        if cancel.is_cancelled() {
            return stopped(i);
        }

        let mut window = keyboard.foreground();
        if window.is_some_and(|w| keyboard.is_own(w)) {
            report(if layout.is_none() { Progress::WaitingForTarget } else { Progress::PausedOwnWindow });
            let paused = Instant::now();
            loop {
                window = keyboard.foreground();
                if !window.is_some_and(|w| keyboard.is_own(w)) {
                    break;
                }
                if cancel.sleep(Duration::from_millis(100)) {
                    return stopped(i);
                }
            }
            // Time spent paused doesn't count towards the estimate.
            started += paused.elapsed();
            last_report = None;
        }

        let layout = *layout.get_or_insert_with(|| {
            started = Instant::now();
            keyboard.layout(window)
        });
        match target {
            None => target = window,
            Some(target) if plan.stop_on_focus_change && window.is_some_and(|w| w != target) => {
                return Outcome::FocusMoved { typed: i };
            }
            Some(_) => {}
        }

        let delay = rng.between(plan.min_delay, plan.max_delay);
        let hold = rng.between(Duration::from_millis(10), Duration::from_millis(30)).min(delay / 2);
        let mut refused = false;
        while keyboard.type_char(c, layout, hold).is_err() {
            if !refused {
                refused = true;
                report(Progress::PausedRefused);
            }
            if cancel.sleep(Duration::from_millis(500)) {
                return stopped(i);
            }
        }
        if refused {
            last_report = None;
        }

        let now = Instant::now();
        let done = i + 1;
        if last_report.is_none_or(|at| now - at >= Duration::from_millis(100)) || done == total {
            last_report = Some(now);
            let elapsed = now - started;
            let left =
                (elapsed >= Duration::from_secs(2)).then(|| elapsed.mul_f64((total - done) as f64 / done as f64));
            report(Progress::Typing { done, total, left });
        }
        if cancel.sleep(delay.saturating_sub(hold)) && done < total {
            return Outcome::Stopped { typed: done };
        }
    }
    Outcome::Done { typed: total }
}

/// Where typing stopped, for "stopped in line 12" (1-based, counting `\n`s before `typed`).
pub fn line_of(text: &str, typed: usize) -> usize {
    text.chars().take(typed).filter(|&c| c == '\n').count() + 1
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    /// Windows: 0 = FerroType's own, anything else is another program's.
    struct Fake {
        windows: Vec<Option<u32>>,
        polls: Cell<usize>,
        typed: RefCell<String>,
        /// Refuse this many keystrokes when this many characters have been typed.
        refuse_at: Option<usize>,
        refusals: Cell<usize>,
        cancel_after: Option<(usize, Cancel)>,
    }

    impl Fake {
        fn new(windows: &[Option<u32>]) -> Fake {
            Fake {
                windows: windows.to_vec(),
                polls: Cell::new(0),
                typed: RefCell::new(String::new()),
                refuse_at: None,
                refusals: Cell::new(0),
                cancel_after: None,
            }
        }
    }

    impl Keyboard for Fake {
        type Window = u32;
        type Layout = ();

        fn foreground(&self) -> Option<u32> {
            let n = self.polls.get();
            self.polls.set(n + 1);
            self.windows[n.min(self.windows.len() - 1)]
        }

        fn is_own(&self, window: u32) -> bool {
            window == 0
        }

        fn layout(&self, _: Option<u32>) {}

        fn modifiers_held(&self) -> bool {
            false
        }

        fn type_char(&self, c: char, _: (), _: Duration) -> Result<(), Refused> {
            if self.refuse_at == Some(self.typed.borrow().chars().count()) && self.refusals.get() < 2 {
                self.refusals.set(self.refusals.get() + 1);
                return Err(Refused);
            }
            self.typed.borrow_mut().push(c);
            if let Some((after, cancel)) = &self.cancel_after
                && self.typed.borrow().chars().count() == *after
            {
                cancel.cancel();
            }
            Ok(())
        }
    }

    fn plan(text: &str) -> Plan {
        Plan {
            text: text.to_owned(),
            min_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            countdown: 0,
            stop_on_focus_change: false,
        }
    }

    fn run_fake(fake: &Fake, plan: &Plan, cancel: &Cancel) -> (Outcome, Vec<Progress>) {
        let mut seen = Vec::new();
        let outcome = run(fake, plan, cancel, &mut Rng::new(7), |p| seen.push(p));
        (outcome, seen)
    }

    #[test]
    fn keeps_typing_across_window_switches() {
        let fake = Fake::new(&[Some(0), Some(0), Some(0), Some(1), Some(1), Some(2), None, Some(2)]);
        let (outcome, seen) = run_fake(&fake, &plan("hello\nworld"), &Cancel::new());
        assert_eq!(outcome, Outcome::Done { typed: 11 });
        assert_eq!(*fake.typed.borrow(), "hello\nworld");
        assert_eq!(seen[0], Progress::WaitingForTarget);
        assert_eq!(*seen.last().unwrap(), Progress::Typing { done: 11, total: 11, left: None });
    }

    #[test]
    fn pauses_while_its_own_window_has_focus() {
        let fake = Fake::new(&[Some(1), Some(1), Some(0), Some(0), Some(0), Some(1)]);
        let (outcome, seen) = run_fake(&fake, &plan("abcdef"), &Cancel::new());
        assert_eq!(outcome, Outcome::Done { typed: 6 });
        assert!(seen.contains(&Progress::PausedOwnWindow));
        assert_eq!(*fake.typed.borrow(), "abcdef");
    }

    #[test]
    fn can_stop_when_focus_moves() {
        let fake = Fake::new(&[Some(1), Some(1), Some(1), Some(2)]);
        let plan = Plan { stop_on_focus_change: true, ..plan("abcdef") };
        let (outcome, _) = run_fake(&fake, &plan, &Cancel::new());
        assert_eq!(outcome, Outcome::FocusMoved { typed: 3 });
        assert_eq!(*fake.typed.borrow(), "abc");
    }

    #[test]
    fn retries_refused_keystrokes_without_doubling() {
        let mut fake = Fake::new(&[Some(1)]);
        fake.refuse_at = Some(3);
        let (outcome, seen) = run_fake(&fake, &plan("abcdef"), &Cancel::new());
        assert_eq!(outcome, Outcome::Done { typed: 6 });
        assert_eq!(*fake.typed.borrow(), "abcdef");
        assert!(seen.contains(&Progress::PausedRefused));
    }

    #[test]
    fn stop_reports_how_far_it_got() {
        let cancel = Cancel::new();
        let mut fake = Fake::new(&[Some(1)]);
        fake.cancel_after = Some((4, cancel.clone()));
        let (outcome, _) = run_fake(&fake, &plan("abcdefgh"), &cancel);
        assert_eq!(outcome, Outcome::Stopped { typed: 4 });
        assert_eq!(line_of("ab\ncd\nef", 4), 2);
    }

    #[test]
    fn stop_while_waiting_for_the_target_is_a_cancel() {
        let cancel = Cancel::new();
        let canceller = cancel.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            canceller.cancel();
        });
        let fake = Fake::new(&[Some(0)]);
        let (outcome, seen) = run_fake(&fake, &plan("abc"), &cancel);
        stopper.join().unwrap();
        assert_eq!(outcome, Outcome::Cancelled);
        assert_eq!(seen, [Progress::WaitingForTarget]);
        assert_eq!(*fake.typed.borrow(), "");
    }

    #[test]
    fn countdown_then_cancel() {
        let cancel = Cancel::new();
        cancel.cancel();
        let fake = Fake::new(&[Some(1)]);
        let plan = Plan { countdown: 3, ..plan("abc") };
        let (outcome, seen) = run_fake(&fake, &plan, &cancel);
        assert_eq!(outcome, Outcome::Cancelled);
        assert_eq!(seen, [Progress::Countdown(3)]);
    }
}
