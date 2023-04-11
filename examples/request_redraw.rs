#![allow(clippy::single_match)]

use simple_logger::SimpleLogger;
use winit::{
    event::{ElementState, Event, WindowEvent},
    event_loop::EventLoop,
    window::WindowBuilder,
};

#[path = "util/fill.rs"]
mod fill;

fn main() -> std::result::Result<(), impl std::error::Error> {
    SimpleLogger::new().init().unwrap();
    let event_loop = EventLoop::new();

    let window = WindowBuilder::new()
        .with_title("A fantastic window!")
        .build(&event_loop)
        .unwrap();

    event_loop.run(move |event, _, control_flow| {
        println!("{event:?}");

        control_flow.set_wait();

        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => control_flow.set_exit(),
                WindowEvent::MouseInput {
                    state: ElementState::Released,
                    ..
                } => {
                    window.request_redraw();
                }
                _ => (),
            },
            Event::RedrawRequested(_) => {
                println!("\nredrawing!\n");
                fill::fill_window(&window);
            }
            _ => (),
        }
    })
}
