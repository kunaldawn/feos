//! Frame-to-frame interaction tests.

use feos_gfx::{Canvas, Point, Rect};
use feos_gui::prelude::*;
use feos_gui::{Chart, ChartPoint, InteractionState, Series, WindowState};

const WIDTH: usize = 320;
const HEIGHT: usize = 240;

fn canvas(pixels: &mut [u32]) -> Canvas<'_> {
    Canvas::new(pixels, WIDTH, HEIGHT, WIDTH).unwrap()
}

#[test]
fn a_button_clicks_on_release() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();

    input.push(Event::PointerDown {
        pos: Point::new(10, 10),
        button: MouseButton::Left,
    });
    let rect = {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let response = ui.button_sized(80, "OK");
        assert!(response.hovered);
        assert!(response.pressed);
        assert!(response.focused);
        response.rect
    };
    assert_eq!(rect, Rect::new(0, 0, 80, 34));
    input.clear_events();

    input.push(Event::PointerUp {
        pos: Point::new(10, 10),
        button: MouseButton::Left,
    });
    let clicked = {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.button_sized(80, "OK").clicked
    };
    assert!(clicked);
}

#[test]
fn tab_and_space_drive_keyboard_activation() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();

    // First frame establishes the tab order.
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.button_sized(80, "one");
        ui.button_sized(80, "two");
    }

    input.push(Event::KeyDown(Key::Tab, Modifiers::NONE));
    let first = {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let first = ui.button_sized(80, "one").focused;
        ui.button_sized(80, "two");
        first
    };
    assert!(first);
    input.clear_events();

    input.push(Event::KeyDown(Key::Tab, Modifiers::NONE));
    let second = {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let first = ui.button_sized(80, "one").focused;
        let second = ui.button_sized(80, "two").focused;
        (first, second)
    };
    assert_eq!(second, (false, true));
    input.clear_events();

    input.push(Event::KeyDown(Key::Space, Modifiers::NONE));
    let activated = {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.button_sized(80, "one");
        ui.button_sized(80, "two").clicked
    };
    assert!(activated);
}

#[test]
fn text_input_edits_and_submits() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut buffer = TextBuffer::<32>::new();

    input.push(Event::PointerDown {
        pos: Point::new(10, 10),
        button: MouseButton::Left,
    });
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let response = ui.text_input(&mut buffer);
        assert!(response.focused);
    }
    input.clear_events();

    input.push(Event::PointerUp {
        pos: Point::new(10, 10),
        button: MouseButton::Left,
    });
    input.push(Event::Text('h'));
    input.push(Event::Text('i'));
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let response = ui.text_input(&mut buffer);
        assert!(response.focused);
        assert!(response.changed);
    }
    assert_eq!(buffer.as_str(), "hi");
    input.clear_events();

    input.push(Event::KeyDown(Key::Enter, Modifiers::NONE));
    let submitted = {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.text_input(&mut buffer).submitted
    };
    assert!(submitted);
    assert_eq!(buffer.as_str(), "hi");
}

#[test]
fn slider_follows_a_drag() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut value = 0.0f32;

    input.push(Event::PointerDown {
        pos: Point::new(100, 17),
        button: MouseButton::Left,
    });
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let response = ui.slider(&mut value, 0.0, 1.0);
        assert!(response.changed);
    }
    assert!((value - 0.30).abs() < 0.05, "value was {value}");
    input.clear_events();

    input.push(Event::PointerMoved(Point::new(300, 17)));
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let response = ui.slider(&mut value, 0.0, 1.0);
        assert!(response.changed);
        assert!(response.dragged);
    }
    assert!(value > 0.9, "value was {value}");
}

#[test]
fn window_title_bar_drag_moves_window() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let mut window = WindowState::new(Rect::new(10, 10, 200, 150));

    input.push(Event::PointerDown {
        pos: Point::new(30, 25),
        button: MouseButton::Left,
    });
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.window(&mut window, "Demo", |ui| {
            ui.label("body");
        });
    }
    input.clear_events();

    input.push(Event::PointerMoved(Point::new(50, 45)));
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        let response = ui.window(&mut window, "Demo", |ui| {
            ui.label("body");
        });
        assert!(response.response.dragged);
    }
    assert_eq!(window.rect.x, 30);
    assert_eq!(window.rect.y, 30);
}

#[test]
fn wheel_scrolls_an_area() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let mut input = Input::new();
    let mut state = InteractionState::<8>::new();
    let id = Id::new(77);

    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.scroll_area_with_id(id, Rect::new(0, 0, 100, 50), |ui| {
            for _ in 0..10 {
                ui.space(20);
            }
        });
    }
    assert_eq!(state.scroll_content(id), 290);

    input.push(Event::PointerMoved(Point::new(10, 10)));
    input.push(Event::PointerWheel(-3));
    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.scroll_area_with_id(id, Rect::new(0, 0, 100, 50), |ui| {
            for _ in 0..10 {
                ui.space(20);
            }
        });
    }
    assert!(state.scroll_offset(id) > 0);
}

#[test]
fn chart_draws_its_series() {
    let mut pixels = [0u32; WIDTH * HEIGHT];
    let theme = Theme::dark();
    let input = Input::new();
    let mut state = InteractionState::<4>::new();
    let points = [
        ChartPoint::new(0.0, 10.0),
        ChartPoint::new(1.0, 40.0),
        ChartPoint::new(2.0, 25.0),
        ChartPoint::new(3.0, 80.0),
        ChartPoint::new(4.0, 60.0),
    ];
    let series = [Series::line(&points, Color::RED)];
    let chart = Chart::new(&series).x_range(0.0, 4.0).y_range(0.0, 100.0);

    {
        let mut canvas = canvas(&mut pixels);
        let mut ui = Ui::new(&mut canvas, &theme, &input, &mut state);
        ui.chart(Rect::new(0, 0, 300, 200), &chart);
    }

    assert!(pixels.iter().any(|pixel| *pixel == Color::RED.raw()));
}
