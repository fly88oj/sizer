//! Executor tests against an in-memory window host.

use std::cell::RefCell;

use sizer_core::{Rect, SizeBasis};

use crate::exec::{
    ExecError, MonitorSnapshot, WindowGeometry, WindowHost, WindowSnapshot, execute,
};
use crate::parse;

#[derive(Default)]
struct Fake {
    windows: RefCell<Vec<WindowSnapshot>>,
    active: Option<String>,
    last: Option<String>,
    desktop: u32,
    current_monitor: usize,
    /// Requests the executor made, in order.
    requests: RefCell<Vec<(String, Rect, SizeBasis)>>,
    /// States the executor asked for, in order.
    states: RefCell<Vec<(String, &'static str, bool)>>,
    /// Refuse geometry below this client size, like a toolkit minimum size.
    minimum: Option<(i32, i32)>,
}

impl Fake {
    fn new(windows: Vec<WindowSnapshot>) -> Self {
        let active = windows.first().map(|window| window.id.clone());
        Self {
            windows: RefCell::new(windows),
            active,
            current_monitor: 0,
            ..Self::default()
        }
    }

    fn window(id: &str, title: &str, class: &str, client: Rect) -> WindowSnapshot {
        WindowSnapshot {
            id: id.to_string(),
            title: title.to_string(),
            class: Some(class.to_string()),
            instance: Some(class.to_lowercase()),
            pid: None,
            desktop: 0,
            client,
            frame: Rect::new(client.x - 2, client.y - 30, client.w + 4, client.h + 32),
            maximized: false,
            minimized: false,
            monitor: 0,
            focused: false,
        }
    }

    fn geometry_of(&self, id: &str) -> WindowGeometry {
        let windows = self.windows.borrow();
        let window = windows.iter().find(|window| window.id == id).unwrap();
        WindowGeometry {
            client: window.client,
            frame: window.frame,
        }
    }
}

impl WindowHost for Fake {
    fn windows(&self) -> Result<Vec<WindowSnapshot>, ExecError> {
        Ok(self.windows.borrow().clone())
    }

    fn active_window(&self) -> Result<Option<WindowSnapshot>, ExecError> {
        Ok(self
            .active
            .as_ref()
            .and_then(|id| self.windows.borrow().iter().find(|w| &w.id == id).cloned()))
    }

    fn last_window(&self) -> Result<Option<WindowSnapshot>, ExecError> {
        Ok(self
            .last
            .as_ref()
            .and_then(|id| self.windows.borrow().iter().find(|w| &w.id == id).cloned()))
    }

    fn monitors(&self) -> Result<Vec<MonitorSnapshot>, ExecError> {
        Ok(vec![MonitorSnapshot {
            rect: Rect::new(0, 0, 1600, 900),
            work_area: Rect::new(0, 30, 1600, 870),
            primary: true,
        }])
    }

    fn current_monitor(&self) -> Result<usize, ExecError> {
        Ok(self.current_monitor)
    }

    fn current_desktop(&self) -> Result<u32, ExecError> {
        Ok(self.desktop)
    }

    fn geometry(&self, id: &str) -> Result<WindowGeometry, ExecError> {
        Ok(self.geometry_of(id))
    }

    fn apply(&self, id: &str, target: Rect, basis: SizeBasis) -> Result<WindowGeometry, ExecError> {
        self.requests
            .borrow_mut()
            .push((id.to_string(), target, basis));
        let mut windows = self.windows.borrow_mut();
        let window = windows
            .iter_mut()
            .find(|window| window.id == id)
            .ok_or_else(|| ExecError::new(format!("no such window {id}")))?;
        let mut client = match basis {
            SizeBasis::Client => target,
            SizeBasis::Frame => Rect::new(target.x + 2, target.y + 30, target.w - 4, target.h - 32),
        };
        if let Some((min_w, min_h)) = self.minimum {
            client.w = client.w.max(min_w);
            client.h = client.h.max(min_h);
        }
        window.client = client;
        window.frame = Rect::new(client.x - 2, client.y - 30, client.w + 4, client.h + 32);
        Ok(WindowGeometry {
            client: window.client,
            frame: window.frame,
        })
    }

    fn set_maximized(&self, id: &str, maximized: bool) -> Result<(), ExecError> {
        self.states
            .borrow_mut()
            .push((id.to_string(), "maximized", maximized));
        Ok(())
    }

    fn set_minimized(&self, id: &str, minimized: bool) -> Result<(), ExecError> {
        self.states
            .borrow_mut()
            .push((id.to_string(), "minimized", minimized));
        Ok(())
    }
}

fn eight_windows() -> Vec<WindowSnapshot> {
    (1..=8)
        .map(|index| {
            Fake::window(
                &index.to_string(),
                &format!("notepad {index}"),
                "Notepad",
                Rect::new(0, 0, 200, 200),
            )
        })
        .collect()
}

#[test]
fn lays_out_the_official_four_column_example() {
    let fake = Fake::new(eight_windows());
    let mac = parse(
        "resize : proc[notepad]*\n\
         t = 300 * (($_index - 1) \\ 4)\n\
         l = 400 * (($_index - 1) % 4)\n\
         w = 400\n\
         h = 300",
    )
    .unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert_eq!(report.windows.len(), 8);
    for (position, outcome) in report.windows.iter().enumerate() {
        let index = position as i32;
        assert_eq!(outcome.index as i32, index + 1);
        assert_eq!(outcome.count, 8);
        assert!(outcome.exact, "{outcome:?}");
        assert_eq!(
            outcome.client,
            Rect::new(400 * (index % 4), 300 * (index / 4), 400, 300),
            "window {index}"
        );
    }
    assert!(
        fake.requests.borrow().iter().all(|(_, rect, basis)| {
            *basis == SizeBasis::Client && rect.w == 400 && rect.h == 300
        })
    );
}

#[test]
fn without_a_star_only_the_first_match_moves() {
    let fake = Fake::new(eight_windows());
    let mac = parse("resize : cap[notepad] w = 500").unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert_eq!(report.windows.len(), 1);
    assert_eq!(report.windows[0].index, 1);
    // `$_count` still reports every match even when only one is processed.
    assert_eq!(report.windows[0].count, 8);
    assert_eq!(report.windows[0].client.w, 500);
    assert_eq!(fake.requests.borrow().len(), 1);
}

#[test]
fn a_missing_window_is_an_error_unless_runtime_errors_are_ignored() {
    let fake = Fake::new(eight_windows());
    let mac = parse("resize : proc[firefox]* w = 500").unwrap();
    let error = execute(&mac, &fake).unwrap_err();
    assert!(error.message.contains("no window matched"), "{error}");
    assert_eq!(error.span.unwrap().line, 1);

    let forgiving = Fake::new(eight_windows());
    let mac = parse("option ignore_runtime_errors on\nresize : proc[firefox]* w = 500").unwrap();
    let report = execute(&mac, &forgiving).unwrap();
    assert!(report.windows.is_empty());
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].reason.contains("skipped"));
}

#[test]
fn last_active_and_desktop_filters_select_the_right_window() {
    let mut fake = Fake::new(vec![
        Fake::window("1", "one", "App", Rect::new(0, 0, 100, 100)),
        Fake::window("2", "two", "App", Rect::new(0, 0, 100, 100)),
        Fake::window("3", "three", "App", Rect::new(0, 0, 100, 100)),
    ]);
    fake.active = Some("2".to_string());
    fake.last = Some("1".to_string());
    fake.desktop = 1;
    fake.windows.borrow_mut()[2].desktop = 2;

    let mac = parse("resize : active w = 300").unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert_eq!(report.windows[0].id, "2");

    let mac = parse("resize : last w = 310").unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert_eq!(report.windows[0].id, "1");

    let mac = parse("resize : cap[three]:desktop w = 320").unwrap();
    let error = execute(&mac, &fake).unwrap_err();
    assert!(error.message.contains("no window matched"), "{error}");

    let mac = parse("resize : cap[three] w = 320").unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert_eq!(report.windows[0].id, "3");

    let mac = parse("resize : active:cap[one] w = 320").unwrap();
    let error = execute(&mac, &fake).unwrap_err();
    assert!(error.message.contains("active:cap[one]"), "{error}");
}

#[test]
fn indexed_filters_pick_one_match() {
    let fake = Fake::new(eight_windows());
    let mac = parse("resize : cap[notepad]:2 w = 640").unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert_eq!(report.windows[0].id, "2");
    assert_eq!(report.windows[0].count, 1);

    let mac = parse("resize : cap[notepad]:9 w = 640").unwrap();
    assert!(execute(&mac, &fake).is_err());
}

#[test]
fn dimensions_read_the_monitor_work_area_and_other_windows() {
    let fake = Fake::new(vec![
        Fake::window("1", "one", "App", Rect::new(10, 40, 200, 100)),
        Fake::window("2", "two", "App", Rect::new(0, 0, 320, 240)),
    ]);
    let mac =
        parse("resize : cap[one] w = w:workarea / 2 h = h:client:cap[two] l = l:mon + 5").unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert_eq!(report.windows[0].client, Rect::new(5, 40, 800, 240));
}

#[test]
fn compensate_frame_effects_assigns_frame_coordinates() {
    let fake = Fake::new(vec![Fake::window(
        "1",
        "one",
        "App",
        Rect::new(0, 0, 200, 100),
    )]);
    let mac = parse(
        "option compensate_frame_effects on\n\
         option ignore_runtime_errors off\n\
         resize : active t = 100 l = 200 w = 400 h = 300",
    )
    .unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert_eq!(report.windows[0].basis, SizeBasis::Frame);
    assert_eq!(report.windows[0].frame, Rect::new(200, 100, 400, 300));
    assert!(report.windows[0].exact);
    let requests = fake.requests.borrow();
    assert_eq!(requests[0].2, SizeBasis::Frame);
    assert_eq!(requests[0].1, Rect::new(200, 100, 400, 300));
}

#[test]
fn variables_are_evaluated_per_window() {
    let fake = Fake::new(eight_windows());
    let mac = parse(
        "$margin = 10\n\
         resize : cap[notepad]* l = $margin * $_index w = $_count * 100",
    )
    .unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert_eq!(report.windows.len(), 8);
    assert_eq!(report.windows[0].client.x, 10);
    assert_eq!(report.windows[3].client.x, 40);
    assert!(report.windows.iter().all(|outcome| outcome.client.w == 800));
}

#[test]
fn state_actions_reach_the_host_in_order() {
    let fake = Fake::new(vec![Fake::window(
        "1",
        "one",
        "App",
        Rect::new(0, 0, 200, 100),
    )]);
    let mac = parse("resize : active minimize restore maximize:primary").unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert!(report.windows[0].requested.is_none());
    assert_eq!(
        *fake.states.borrow(),
        vec![
            ("1".to_string(), "minimized", true),
            // `restore` only turns off state the block left behind.
            ("1".to_string(), "minimized", false),
            ("1".to_string(), "maximized", true),
        ]
    );
    // `maximize:primary` names the monitor the window is already on, so the
    // window is not moved first.
    assert!(fake.requests.borrow().is_empty());
}

#[test]
fn restore_clears_the_state_the_window_already_had() {
    let fake = Fake::new(vec![Fake::window(
        "1",
        "one",
        "App",
        Rect::new(0, 0, 200, 100),
    )]);
    {
        let mut windows = fake.windows.borrow_mut();
        windows[0].maximized = true;
        windows[0].minimized = true;
    }
    let report = execute(&parse("resize : active restore").unwrap(), &fake).unwrap();
    assert!(report.windows[0].requested.is_none());
    assert_eq!(
        *fake.states.borrow(),
        vec![
            ("1".to_string(), "minimized", false),
            ("1".to_string(), "maximized", false),
        ]
    );
}

#[test]
fn a_window_manager_adjustment_is_reported_not_hidden() {
    let mut fake = Fake::new(vec![Fake::window(
        "1",
        "one",
        "App",
        Rect::new(0, 0, 200, 100),
    )]);
    fake.minimum = Some((500, 400));
    let mac = parse("resize : active w = 100 h = 100").unwrap();
    let report = execute(&mac, &fake).unwrap();
    assert!(!report.windows[0].exact);
    assert!(!report.exact());
    assert_eq!(report.windows[0].client.w, 500);
    assert_eq!(report.windows[0].client.h, 400);
}

#[test]
fn reports_are_stable_json() {
    let fake = Fake::new(vec![Fake::window(
        "1",
        "one",
        "App",
        Rect::new(0, 0, 200, 100),
    )]);
    let report = execute(&parse("resize : active w = 300").unwrap(), &fake).unwrap();
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["windows"][0]["id"], "1");
    assert_eq!(json["windows"][0]["requested"]["w"], 300);
    assert_eq!(json["windows"][0]["exact"], true);
    // `error` is omitted when absent instead of appearing as null.
    assert!(json["windows"][0].get("error").is_none());
    assert!(json.get("skipped").is_some());
}
