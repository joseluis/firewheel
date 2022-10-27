use firewheel::{AppWindow, PhysicalSize};
use raw_gl_context::{GlConfig, GlContext};
use winit::{
    event::*,
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};

fn main() {
    env_logger::init();
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new().build(&event_loop).unwrap();

    let gl_config = GlConfig {
        vsync: true,
        ..Default::default()
    };

    let context = GlContext::create(&window, gl_config).unwrap();
    context.make_current();
    let mut app_window = unsafe {
        AppWindow::<()>::new_from_function(window.scale_factor().into(), |s| {
            context.get_proc_address(s) as _
        })
    };
    context.make_not_current();

    let mut window_size = PhysicalSize::new(window.inner_size().width, window.inner_size().height);

    let mut msg_out_queue = Vec::new();

    event_loop.run(move |event, _, control_flow| match event {
        Event::WindowEvent {
            ref event,
            window_id,
        } if window_id == window.id() => match event {
            WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
            WindowEvent::Resized(physical_size) => {
                window_size = PhysicalSize::new(physical_size.width, physical_size.height);
            }
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                new_inner_size,
            } => {
                app_window.set_scale_factor((*scale_factor).into(), &mut msg_out_queue);
                window_size = PhysicalSize::new(new_inner_size.width, new_inner_size.height);
            }
            _ => {}
        },
        Event::RedrawRequested(window_id) if window_id == window.id() => {
            context.make_current();

            app_window.render(window_size, [0.06, 0.06, 0.06, 1.0]);

            context.swap_buffers();
            context.make_not_current();
        }
        Event::MainEventsCleared => {
            window.request_redraw();
        }
        _ => {}
    });
}