use crate::{
    event::{Event, PumpStatus},
    event_loop::{ControlFlow, EventLoop, EventLoopWindowTarget},
};

/// Additional methods on [`EventLoop`] for pumping events within an external event loop
pub trait EventLoopExtPumpEvents {
    /// A type provided by the user that can be passed through [`Event::UserEvent`].
    type UserEvent;

    fn pump_events<F>(&mut self, event_handler: F) -> PumpStatus
    where
        F: FnMut(
            Event<'_, Self::UserEvent>,
            &EventLoopWindowTarget<Self::UserEvent>,
            &mut ControlFlow,
        );
}

impl<T> EventLoopExtPumpEvents for EventLoop<T> {
    type UserEvent = T;

    fn pump_events<F>(&mut self, event_handler: F) -> PumpStatus
    where
        F: FnMut(
            Event<'_, Self::UserEvent>,
            &EventLoopWindowTarget<Self::UserEvent>,
            &mut ControlFlow,
        ),
    {
        self.event_loop.pump_events(event_handler)
    }
}
