//! Hand-off of the native window from the Java main thread to the native event loop.
//!
//! This module uses no platform types, so its tests run off-device:
//! `rustc --edition 2021 --test src/native_activity/window_handoff.rs`.

/// Tracks the window Java last reported and the window the application was given.
///
/// The application owns a window from the `InitWindow` it is sent until its `TermWindow`
/// handler returns. Java reports made before the application polls for events are only
/// recorded, so a window that is created and destroyed in that time is never exposed.
#[derive(Clone, Debug)]
pub struct WindowHandoff<W> {
    /// The window Java currently has, or `None` once it reported the window destroyed.
    reported: Option<W>,
    /// The window the application currently owns.
    adopted: Option<W>,
}

/// The commands the Java thread queues after a [`WindowHandoff::report`], in this order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowCmds {
    /// The application owns a window and must release it first.
    pub term: bool,
    /// There is a new window for the application to adopt.
    pub init: bool,
}

impl<W> Default for WindowHandoff<W> {
    fn default() -> Self {
        Self {
            reported: None,
            adopted: None,
        }
    }
}

impl<W: Clone + PartialEq> WindowHandoff<W> {
    /// The window the application owns, if any.
    pub fn adopted(&self) -> Option<&W> {
        self.adopted.as_ref()
    }

    /// The window Java currently has, if any.
    pub fn reported(&self) -> Option<&W> {
        self.reported.as_ref()
    }

    /// Records Java's current window (`None` when destroyed) and returns the commands to queue.
    ///
    /// A window the application never adopted is replaced or withdrawn without a `TermWindow`.
    pub fn report(&mut self, window: Option<W>) -> WindowCmds {
        self.reported = window;
        WindowCmds {
            term: self.adopted.is_some(),
            init: self.reported.is_some(),
        }
    }

    /// Whether the application owns exactly the window Java reported.
    ///
    /// Once the event loop runs, a window callback returns to Java only when this holds, so the
    /// application has stopped using a destroyed window before Java releases it.
    pub fn settled(&self) -> bool {
        self.adopted == self.reported
    }

    /// Handles an `InitWindow` command, returning whether to send it to the application.
    ///
    /// The command is stale, and must be dropped, when its window was destroyed before the
    /// application could adopt it or when an earlier `InitWindow` already delivered the
    /// reported window.
    pub fn adopt(&mut self) -> bool {
        if self.adopted.is_some() || self.reported.is_none() {
            return false;
        }
        self.adopted = self.reported.clone();
        true
    }

    /// Records that the application's `TermWindow` handler returned.
    pub fn release(&mut self) {
        self.adopted = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Cmd {
        Init,
        Term,
        Resized,
        /// Any command that doesn't concern the window, such as `Start`.
        Other,
    }

    /// The glue's window protocol with both threads and the command pipe modelled.
    #[derive(Clone, Debug)]
    struct Model {
        handoff: WindowHandoff<u32>,
        loop_started: bool,
        pipe: VecDeque<Cmd>,
        /// The window Java has, as last passed to a window callback.
        java_window: Option<u32>,
        /// Whether Java is waiting inside a window callback.
        java_blocked: bool,
        next_window: u32,
        /// A command whose handler the application is running.
        handling: Option<Cmd>,
        /// The window the application believes it owns, from the events it received.
        app_window: Option<u32>,
        /// Events sent to the application, with the window it saw.
        delivered: Vec<(Cmd, Option<u32>)>,
    }

    impl Model {
        fn new() -> Self {
            Self {
                handoff: WindowHandoff::default(),
                loop_started: false,
                pipe: VecDeque::new(),
                java_window: None,
                java_blocked: false,
                next_window: 1,
                handling: None,
                app_window: None,
                delivered: Vec::new(),
            }
        }

        /// `onNativeWindowCreated` or `onNativeWindowDestroyed`, as `set_window` implements them.
        fn java_set_window(&mut self, window: Option<u32>) {
            assert!(!self.java_blocked);
            self.java_window = window;
            let cmds = self.handoff.report(window);
            if cmds.term {
                self.pipe.push_back(Cmd::Term);
            }
            if cmds.init {
                self.pipe.push_back(Cmd::Init);
            }
            self.java_blocked = self.loop_started && !self.handoff.settled();
            self.check();
        }

        fn java_create(&mut self) -> u32 {
            let window = self.next_window;
            self.next_window += 1;
            self.java_set_window(Some(window));
            window
        }

        fn java_destroy(&mut self) {
            self.java_set_window(None);
        }

        fn java_resize(&mut self) {
            assert!(!self.java_blocked && self.java_window.is_some());
            self.pipe.push_back(Cmd::Resized);
        }

        fn java_other(&mut self) {
            assert!(!self.java_blocked);
            self.pipe.push_back(Cmd::Other);
        }

        /// The start of a `poll_events` call: reads one command and enters its handler.
        fn native_begin(&mut self) {
            assert!(self.handling.is_none());
            self.loop_started = true;
            let Some(cmd) = self.pipe.pop_front() else {
                return;
            };
            let deliver = match cmd {
                Cmd::Init => self.handoff.adopt(),
                Cmd::Resized => self.handoff.adopted().is_some(),
                Cmd::Term | Cmd::Other => true,
            };
            if deliver {
                let visible = self.handoff.adopted().copied();
                match cmd {
                    Cmd::Init => {
                        assert_eq!(self.app_window, None, "InitWindow while owning a window");
                        assert!(visible.is_some(), "InitWindow without a window");
                        self.app_window = visible;
                    }
                    Cmd::Term => {
                        assert!(self.app_window.is_some(), "TermWindow without a window");
                        assert_eq!(self.app_window, visible);
                    }
                    Cmd::Resized => {
                        assert!(self.app_window.is_some(), "resize without a window");
                        assert_eq!(self.app_window, visible);
                    }
                    Cmd::Other => {}
                }
                self.delivered.push((cmd, visible));
                self.handling = Some(cmd);
            }
            self.wake_java();
        }

        /// The application's handler returns and `poll_events` finishes.
        fn native_end(&mut self) {
            if self.handling.take() == Some(Cmd::Term) {
                self.app_window = None;
                self.handoff.release();
            }
            self.wake_java();
        }

        fn native_poll(&mut self) {
            self.native_begin();
            self.native_end();
        }

        fn wake_java(&mut self) {
            if self.java_blocked && self.handoff.settled() {
                self.java_blocked = false;
            }
            self.check();
        }

        fn check(&self) {
            assert_eq!(self.handoff.adopted().copied(), self.app_window);
            if !self.loop_started {
                assert!(!self.java_blocked, "Java waited before the loop started");
                assert_eq!(
                    self.app_window, None,
                    "window exposed before the loop started"
                );
            }
            if !self.java_blocked {
                // Every returned callback leaves the application without a window, or with the
                // one Java still has.
                if let Some(window) = self.app_window {
                    assert_eq!(
                        Some(window),
                        self.java_window,
                        "application kept a dead window"
                    );
                }
            } else {
                assert!(
                    !self.pipe.is_empty() || self.handling.is_some(),
                    "Java waits with nothing left to wake it"
                );
            }
        }

        /// Runs the native loop until the pipe is empty, then checks both sides agree.
        fn drain(&mut self) {
            if self.handling.is_some() {
                self.native_end();
            }
            while !self.pipe.is_empty() {
                self.native_poll();
            }
            self.native_poll();
            assert!(!self.java_blocked);
            assert_eq!(self.app_window, self.java_window);
        }

        /// Tries every interleaving of Java callbacks and native progress up to `depth` steps.
        fn explore(&self, depth: u32, visited: &mut u64) {
            *visited += 1;
            self.clone().drain();
            if depth == 0 {
                return;
            }
            if !self.java_blocked {
                let mut next = self.clone();
                if self.java_window.is_some() {
                    next.java_destroy();
                    let mut resized = self.clone();
                    resized.java_resize();
                    resized.explore(depth - 1, visited);
                } else {
                    next.java_create();
                }
                next.explore(depth - 1, visited);
            }
            let mut next = self.clone();
            if self.handling.is_some() {
                next.native_end();
            } else {
                next.native_begin();
            }
            next.explore(depth - 1, visited);
        }
    }

    #[test]
    fn window_reported_before_the_loop_is_delivered_once_without_waiting() {
        let mut model = Model::new();
        let window = model.java_create();
        model.java_resize();
        assert!(!model.java_blocked);
        assert_eq!(model.handoff.adopted(), None);

        model.drain();
        assert_eq!(
            model.delivered,
            [(Cmd::Init, Some(window)), (Cmd::Resized, Some(window))]
        );
    }

    #[test]
    fn window_destroyed_before_the_loop_is_never_delivered() {
        let mut model = Model::new();
        model.java_create();
        model.java_resize();
        model.java_destroy();
        assert!(!model.java_blocked);

        model.drain();
        assert!(model.delivered.is_empty());
    }

    #[test]
    fn recreated_window_before_the_loop_delivers_only_the_latest() {
        let mut model = Model::new();
        model.java_create();
        model.java_destroy();
        let latest = model.java_create();

        model.drain();
        assert_eq!(model.delivered, [(Cmd::Init, Some(latest))]);
    }

    #[test]
    fn unadopted_window_is_withdrawn_without_waiting_after_the_loop_started() {
        let mut model = Model::new();
        model.java_other();
        model.java_create();
        model.native_begin();
        assert!(model.loop_started);

        model.java_destroy();
        assert!(!model.java_blocked, "the application never saw this window");
        model.drain();
        assert_eq!(model.delivered, [(Cmd::Other, None)]);
    }

    #[test]
    fn stale_init_delivers_the_replacement_window_once() {
        let mut model = Model::new();
        model.java_other();
        model.java_create();
        model.native_begin();
        model.java_destroy();
        let latest = model.java_create();
        assert!(model.java_blocked);

        // The first `InitWindow` was queued for the destroyed window.
        model.native_end();
        model.native_begin();
        assert!(!model.java_blocked);
        model.drain();
        assert_eq!(
            model.delivered,
            [(Cmd::Other, None), (Cmd::Init, Some(latest))]
        );
    }

    #[test]
    fn adopted_window_blocks_java_until_the_term_handler_returns() {
        let mut model = Model::new();
        let window = model.java_create();
        model.native_poll();
        assert_eq!(model.app_window, Some(window));

        model.java_destroy();
        assert!(model.java_blocked);
        model.native_begin();
        assert!(model.java_blocked, "the handler is still using the window");
        model.native_end();
        assert!(!model.java_blocked);
        assert_eq!(
            model.delivered,
            [(Cmd::Init, Some(window)), (Cmd::Term, Some(window))]
        );
    }

    #[test]
    fn window_created_after_the_loop_started_blocks_java_until_adopted() {
        let mut model = Model::new();
        model.native_poll();
        let window = model.java_create();
        assert!(model.java_blocked);
        model.native_begin();
        assert!(!model.java_blocked);
        assert_eq!(model.app_window, Some(window));
    }

    #[test]
    fn every_interleaving_keeps_the_window_contract() {
        let mut visited = 0;
        // Leading non-window commands let the loop start while window commands are queued.
        for leading in 0..3 {
            let mut model = Model::new();
            for _ in 0..leading {
                model.java_other();
            }
            model.explore(12, &mut visited);
        }
        assert!(visited > 10_000, "explored only {visited} states");
    }
}
