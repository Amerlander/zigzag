//! Frame-time benchmarks for the gestures that must feel smooth: typing
//! on the canvas and dragging objects, on a traced logo (hundreds of
//! shapes, tens of thousands of stitches). Each figure is a whole frame:
//! the UI pass plus egui's tessellation of what it painted, which is
//! where directly drawn stitches cost.
//!
//! `cargo test --release --features gui --bin zigzag-studio perf_bench -- --ignored --nocapture`

#![cfg(test)]

use crate::app::Studio;
use eframe::egui::{self, Vec2};
use egui_kittest::Harness;
use zigzag::geom::P;

fn studio_with_logo() -> Harness<'static, Studio> {
    let png = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/agent-badge-preview.png"
    ));
    let mut h = Harness::builder()
        .with_size(Vec2::new(1600.0, 1000.0))
        .build_ui_state(
            |ui, s: &mut Studio| s.frame(ui),
            Studio::with_file(None, None),
        );
    // As the app does it: a slow plan moves planning to the worker.
    h.state_mut().planner.timed = true;
    let doc = zigzag::document::Document::open(&png).unwrap();
    h.state_mut().set_document(doc);
    h.run_steps(5);
    h
}

/// One frame, tessellated: milliseconds.
fn frame_ms(
    h: &mut Harness<'static, Studio>,
    before: impl FnOnce(&mut Harness<'static, Studio>),
) -> f64 {
    before(h);
    let t = std::time::Instant::now();
    h.step();
    let shapes = h.output().shapes.clone();
    let _ = h.ctx.tessellate(shapes, 1.0);
    t.elapsed().as_secs_f64() * 1000.0
}

fn report(what: &str, ms: &[f64]) {
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    let max = ms.iter().cloned().fold(0.0, f64::max);
    println!("{what:<34} mean {mean:6.2} ms   max {max:6.2} ms");
}

#[test]
#[ignore]
fn typing_and_dragging_on_a_traced_logo() {
    let mut h = studio_with_logo();
    println!(
        "{} shapes, {} stitches",
        h.state().doc.shapes.len(),
        h.state().design.stitch_count()
    );
    let rect = h.state().canvas_rect;

    // Idle, nothing happening.
    let idle: Vec<f64> = (0..10).map(|_| frame_ms(&mut h, |_| {})).collect();
    report("idle", &idle);

    // Type tool: click on the canvas, type a word.
    h.key_press(egui::Key::T);
    h.run_steps(1);
    let at = h.state().view.to_screen(rect, P::new(0.0, 0.0));
    h.hover_at(at);
    h.run_steps(1);
    h.drag_at(at);
    h.run_steps(1);
    h.drop_at(at);
    h.run_steps(2);
    let mut keys = Vec::new();
    for ch in "CAMPFIRE".chars() {
        // The keystroke's frame plus the one after it (the replan lands
        // on the next frame).
        let a = frame_ms(&mut h, |h| h.event(egui::Event::Text(ch.to_string())));
        let b = frame_ms(&mut h, |_| {});
        keys.push(a + b);
    }
    report("keystroke (2 frames)", &keys);
    let idle_typing: Vec<f64> = (0..10).map(|_| frame_ms(&mut h, |_| {})).collect();
    report("idle while typing", &idle_typing);
    // Mouse moving over the canvas while typing (hover repaints).
    let hover_typing: Vec<f64> = (0..10)
        .map(|k| {
            frame_ms(&mut h, |h| {
                h.hover_at(at + Vec2::new(40.0 + k as f32 * 5.0, 60.0))
            })
        })
        .collect();
    report("hover while typing", &hover_typing);
    if std::env::var_os("TL_PROFILE").is_some() {
        eprintln!("-- profiled: a keystroke, the frame after, then idle while typing");
        let _ = frame_ms(&mut h, |h| h.event(egui::Event::Text("S".into())));
        let _ = frame_ms(&mut h, |_| {});
        let _ = frame_ms(&mut h, |_| {});
        let _ = frame_ms(&mut h, |_| {});
    }
    h.key_press(egui::Key::Escape);
    h.run_steps(2);

    // Selection tool: drag the biggest shape.
    h.key_press(egui::Key::V);
    h.run_steps(1);
    let big = (0..h.state().doc.shapes.len())
        .max_by_key(|&i| {
            h.state()
                .doc
                .edit_path(i)
                .map(|p| p[0].anchors.len())
                .unwrap_or(0)
        })
        .unwrap();
    h.state_mut().set_selection(&[big]);
    h.run_steps(2);
    let c = h.state().view.to_screen(
        rect,
        crate::app::shape_bounds(&h.state().doc.shapes[big]).center(),
    );
    h.hover_at(c);
    h.run_steps(1);
    h.drag_at(c);
    h.run_steps(1);
    let drag: Vec<f64> = (1..=20)
        .map(|k| {
            frame_ms(&mut h, |h| {
                h.hover_at(c + Vec2::new(k as f32 * 3.0, k as f32))
            })
        })
        .collect();
    report("drag biggest shape", &drag);
    h.drop_at(c + Vec2::new(60.0, 20.0));
    h.run_steps(3);

    // Drag everything.
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    h.run_steps(2);
    let n = h.state().sel_indices().len();
    let c = h
        .state()
        .view
        .to_screen(rect, h.state().selection_bounds().center());
    h.hover_at(c);
    h.run_steps(1);
    h.drag_at(c);
    h.run_steps(1);
    let drag_all: Vec<f64> = (1..=20)
        .map(|k| frame_ms(&mut h, |h| h.hover_at(c + Vec2::new(k as f32 * 3.0, 0.0))))
        .collect();
    report(&format!("drag all ({n} selected)"), &drag_all);
    if std::env::var_os("TL_PROFILE").is_some() {
        eprintln!("-- profiled: dragging everything");
        let _ = frame_ms(&mut h, |h| h.hover_at(c + Vec2::new(70.0, 0.0)));
        let _ = frame_ms(&mut h, |h| h.hover_at(c + Vec2::new(80.0, 0.0)));
    }
    h.drop_at(c + Vec2::new(60.0, 0.0));
    h.run_steps(3);

    // Pan.
    h.key_press(egui::Key::H);
    h.run_steps(1);
    let p0 = rect.center();
    h.hover_at(p0);
    h.run_steps(1);
    h.drag_at(p0);
    h.run_steps(1);
    let pan: Vec<f64> = (1..=10)
        .map(|k| frame_ms(&mut h, |h| h.hover_at(p0 + Vec2::new(k as f32 * 4.0, 0.0))))
        .collect();
    report("pan", &pan);
    h.drop_at(p0 + Vec2::new(40.0, 0.0));
}

/// Frame times of the everyday gestures on designs of your own: every
/// `.svg` in the folder `ZIGZAG_DESIGNS` names. Each figure is the UI pass
/// of a frame (what [`Studio::frame`] takes; the app draws stitches on the
/// GPU, so the painter's tessellation of them here would say nothing).
/// With `TL_PROFILE=1`, each frame's sections go to stderr as well.
///
/// `ZIGZAG_DESIGNS=<folder> cargo test --release --features gui --bin zigzag-studio perf_bench::gestures -- --ignored --nocapture`
#[test]
#[ignore]
fn gestures_on_designs_of_your_own() {
    let dir = std::env::var("ZIGZAG_DESIGNS").expect("ZIGZAG_DESIGNS: a folder of .svg files");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "svg"))
        .collect();
    files.sort();
    for file in files {
        let name = file.file_stem().unwrap().to_string_lossy().into_owned();
        let mut h = Harness::builder()
            .with_size(Vec2::new(1600.0, 1000.0))
            .build_ui_state(
                |ui, s: &mut Studio| s.frame(ui),
                Studio::with_file(None, None),
            );
        h.state_mut().planner.timed = true;
        let doc = zigzag::document::Document::open(&file).unwrap();
        h.state_mut().set_document(doc);
        h.run_steps(5);
        let step = |h: &mut Harness<'static, Studio>,
                    what: &str,
                    n: usize,
                    f: &mut dyn FnMut(&mut Harness<'static, Studio>, usize)| {
            eprintln!("-- {name}: {what}");
            let ms: Vec<f64> = (0..n)
                .map(|k| {
                    f(h, k);
                    let t = std::time::Instant::now();
                    h.step();
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            report(&format!("  {what}"), &ms);
        };
        // A plan of the whole design, from nothing.
        let t = std::time::Instant::now();
        let fresh = h.state().doc.plan_cached(&mut Default::default());
        println!(
            "{name}: {} objects, {} stitches; a plan from nothing {:.1} ms, the last one {:.1} ms{}",
            h.state().doc.shapes.len(),
            fresh.stitch_count(),
            t.elapsed().as_secs_f64() * 1000.0,
            h.state().plan_ms,
            if h.state().planner.slow {
                " (planned in the background)"
            } else {
                ""
            },
        );
        let rect = h.state().canvas_rect;
        let c = rect.center();
        step(&mut h, "idle", 10, &mut |_, _| {});
        step(&mut h, "pointer moving over the design", 20, &mut |h, k| {
            h.hover_at(c + Vec2::new(k as f32 * 9.0 - 90.0, k as f32 * 5.0 - 50.0))
        });
        // The object with the most corners showing widgets, selected.
        let mut best = (0, 0);
        for i in 0..h.state().doc.shapes.len() {
            h.state_mut().set_selection(&[i]);
            if h.state_mut().editable_path().is_some() {
                let n = h.state().corner_widgets().len();
                if n > best.1 {
                    best = (i, n);
                }
            }
        }
        let (i, n) = best;
        h.state_mut().set_selection(&[i]);
        h.run_steps(2);
        step(
            &mut h,
            &format!("idle, one selected ({n} corner widgets)"),
            10,
            &mut |_, _| {},
        );
        let at = h.state().view.to_screen(
            rect,
            crate::app::shape_bounds(&h.state().doc.shapes[i]).center(),
        );
        step(
            &mut h,
            "pointer moving over the selection",
            20,
            &mut |h, k| h.hover_at(at + Vec2::new(k as f32 * 2.0 - 20.0, k as f32 - 10.0)),
        );
        // Rounding its corners.
        h.state_mut().editable_path();
        if let Some(&(corner, w)) = h.state().corner_widgets().first() {
            let from = h.state().view.to_screen(rect, w);
            let to = h
                .state()
                .view
                .to_screen(rect, w + (corner.u + corner.v).norm() * 30.0);
            h.hover_at(from);
            h.run_steps(1);
            h.drag_at(from);
            h.run_steps(1);
            step(&mut h, "dragging a corner widget", 20, &mut |h, k| {
                h.hover_at(from + (to - from) * ((k + 1) as f32 / 20.0))
            });
            step(&mut h, "letting it go", 1, &mut |h, _| h.drop_at(to));
            h.run_steps(2);
            h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
            h.run_steps(3);
        }
        // Moving it.
        h.hover_at(at);
        h.run_steps(1);
        h.drag_at(at);
        h.run_steps(1);
        step(&mut h, "dragging the object", 20, &mut |h, k| {
            h.hover_at(at + Vec2::new(k as f32 * 3.0 + 3.0, k as f32))
        });
        step(&mut h, "letting it go", 1, &mut |h, _| {
            h.drop_at(at + Vec2::new(60.0, 20.0))
        });
        step(&mut h, "the frames after", 3, &mut |_, _| {});
        // A setting changed (a slider's every step re-plans).
        step(&mut h, "a stitch setting, step by step", 10, &mut |h, k| {
            if let Some(i) = h.state().sel_index() {
                h.state_mut().doc.shapes[i].params.spacing = Some(0.3 + k as f64 * 0.01);
            }
        });
        // Everything selected, and moved.
        h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
        h.run_steps(2);
        let n = h.state().sel_indices().len();
        step(
            &mut h,
            &format!("idle, all {n} selected"),
            10,
            &mut |_, _| {},
        );
        let at = h
            .state()
            .view
            .to_screen(rect, h.state().selection_bounds().center());
        h.hover_at(at);
        h.run_steps(1);
        h.drag_at(at);
        h.run_steps(1);
        step(&mut h, "dragging everything", 20, &mut |h, k| {
            h.hover_at(at + Vec2::new(k as f32 * 3.0 + 3.0, 0.0))
        });
        step(&mut h, "letting it go", 1, &mut |h, _| {
            h.drop_at(at + Vec2::new(60.0, 0.0))
        });
        step(&mut h, "the frames after", 3, &mut |_, _| {});
        // The recovery copy written (it is after every edit settles).
        let t = std::time::Instant::now();
        let svg = h.state().doc.to_svg().unwrap();
        println!(
            "  writing it out as SVG                {:6.2} ms ({} KB)",
            t.elapsed().as_secs_f64() * 1000.0,
            svg.len() / 1024
        );
    }
}

/// Planning a design of your own from nothing, over and over (to look at
/// with a sampling profiler), and how long each object takes by itself.
///
/// `ZIGZAG_DESIGN=<file.svg> cargo test --release --features gui --bin zigzag-studio perf_bench::planning -- --ignored --nocapture`
#[test]
#[ignore]
fn planning_a_design_of_your_own() {
    let file = std::env::var("ZIGZAG_DESIGN").expect("ZIGZAG_DESIGN: an .svg file");
    let doc = zigzag::document::Document::open(std::path::Path::new(&file)).unwrap();
    let names = zigzag::svg::display_names(&doc.shapes);
    let opt = doc.plan_options();
    for (i, s) in doc.shapes.iter().enumerate() {
        let t = std::time::Instant::now();
        let d = zigzag::design::plan_cached(
            "one",
            std::slice::from_ref(s),
            &opt,
            &mut Default::default(),
        );
        println!(
            "{:>8.2} ms  {:>6} stitches  {} ({:?})",
            t.elapsed().as_secs_f64() * 1000.0,
            d.stitch_count(),
            names[i],
            s.params.kind
        );
    }
    let secs: f64 = std::env::var("ZIGZAG_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let start = std::time::Instant::now();
    let mut n = 0;
    let mut total = 0.0;
    loop {
        let t = std::time::Instant::now();
        let d = doc.plan_cached(&mut Default::default());
        total += t.elapsed().as_secs_f64() * 1000.0;
        n += 1;
        std::hint::black_box(d);
        if start.elapsed().as_secs_f64() >= secs {
            break;
        }
    }
    println!(
        "the whole design from nothing: {:.2} ms (mean of {n})",
        total / n as f64
    );
    let d = doc.plan_cached(&mut Default::default());
    for (k, b) in d.blocks.iter().enumerate() {
        println!(
            "block {k}: thread {:?}, {} runs, objects {:?}",
            b.color,
            b.runs.len(),
            d.block_shapes[k]
                .iter()
                .map(|i| names[*i].as_str())
                .collect::<Vec<_>>()
        );
    }
    let t = std::time::Instant::now();
    let issues = zigzag::checks::check(&doc.shapes, &d, &opt, None);
    println!(
        "checks: {:.2} ms, {} issues",
        t.elapsed().as_secs_f64() * 1000.0,
        issues.len()
    );
}
