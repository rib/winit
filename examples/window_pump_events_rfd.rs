#![allow(clippy::single_match)]

// Limit this example to only compatible platforms.
#[cfg(any(windows_platform, macos_platform, x11_platform, wayland_platform,))]
fn main() -> std::process::ExitCode {
    use std::{process::ExitCode, thread::sleep, time::Duration};

    use simple_logger::SimpleLogger;
    use winit::{
        event::{Event, PumpStatus, WindowEvent},
        event_loop::EventLoop,
        platform::pump_events::EventLoopExtPumpEvents,
        window::WindowBuilder,
    };
    let mut event_loop = EventLoop::new();

    SimpleLogger::new().init().unwrap();
    let window = WindowBuilder::new()
        .with_title("A fantastic window!")
        .build(&event_loop)
        .unwrap();

    'main: loop {
        let status = event_loop.pump_events(|event, _, control_flow| {
            if let Event::WindowEvent { event, .. } = &event {
                // Print only Window events to reduce noise
                println!("{:?}", event);
            }

            match event {
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    window_id,
                } if window_id == window.id() => control_flow.set_exit(),
                Event::MainEventsCleared => {
                    window.request_redraw();
                }
                _ => (),
            }
        });
        if let PumpStatus::Exit(exit_code) = status {
            break 'main ExitCode::from(exit_code as u8);
        }

        // Sleep for 1/60 second to simulate rendering
        println!("rendering");

        let _dialog = rfd::MessageDialog::new()
            .set_title("Msg!")
            .set_description("Description!")
            .set_buttons(rfd::MessageButtons::YesNo)
            .show();

        sleep(Duration::from_millis(33));
    }
}

#[cfg(any(target_os = "ios", target_arch = "wasm32"))]
fn main() {
    println!("This platform doesn't support pump_events.");
}
