use crate::{
    error::ExternalError,
    event::Event,
    event_loop::{ControlFlow, EventLoop, EventLoopWindowTarget},
};

/// Additional methods on [`EventLoop`] to return control flow to the caller.
pub trait EventLoopExtRunOnDemand {
    /// A type provided by the user that can be passed through [`Event::UserEvent`].
    type UserEvent;

    /// Runs the event loop in the calling thread and calls the given `event_handler` closure
    /// to dispatch any window system events.
    ///
    /// Unlike [`EventLoop::run`], this function accepts non-`'static` (i.e. non-`move`) closures
    /// and it is possible to return control back to the caller without
    /// consuming the `EventLoop` (by setting the `control_flow` to [`ControlFlow::Exit`]) and
    /// so the event loop can be re-run after it has exit.
    ///
    /// It's expected that each run of the loop will be for orthogonal instantiations of your
    /// Winit application, but internally each instantiation may re-use some common window
    /// system resources, such as a display server connection.
    ///
    /// This API is not designed to run an event loop in bursts that you can exit from and return
    /// to while maintaining the full state of your application. (If you need something like this
    /// you can look at the [`pump_events()`] API)
    ///
    /// Each time `run_ondemand` is called the `event_handler` can expect to receive a
    /// `NewEvents(Init)` and `Resumed` event (even on platforms that have no suspend/resume
    /// lifecycle) - which can be used to consistently initialize application state.
    ///
    /// See the [`ControlFlow`] docs for information on how changes to `&mut ControlFlow` impact the
    /// event loop's behavior.
    ///
    /// # Caveats
    /// - This extension isn't available on all platforms, since it's not always possible to
    ///   return to the caller (specifically this is impossible on iOS and Web - though with
    ///   the Web backend it is possible to use `spawn()` more than once instead).
    /// - No [`Window`] state can be carried between separate runs of the event loop.
    ///
    /// You are strongly encouraged to use `run` for portability, unless you specifically need
    /// the ability to re-run a single event loop more than once
    ///
    /// ## Platform-specific
    ///
    ///  See the platform specific notes for [`EventLoop::run`]
    ///
    /// - **Android:** Although this API could technically be supported on Android, it's currently
    ///   not exposed because it's very unclear how it could be used meaningfully.
    fn run_ondemand<F>(&mut self, event_handler: F) -> Result<(), ExternalError>
    where
        F: FnMut(
            Event<'_, Self::UserEvent>,
            &EventLoopWindowTarget<Self::UserEvent>,
            &mut ControlFlow,
        );
}

impl<T> EventLoopExtRunOnDemand for EventLoop<T> {
    type UserEvent = T;

    fn run_ondemand<F>(&mut self, event_handler: F) -> Result<(), ExternalError>
    where
        F: FnMut(
            Event<'_, Self::UserEvent>,
            &EventLoopWindowTarget<Self::UserEvent>,
            &mut ControlFlow,
        ),
    {
        self.event_loop.run_ondemand(event_handler)
    }
}
