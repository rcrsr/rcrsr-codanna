/// Simple progress templates - self-contained demo
/// Simulates qtbase case: 8.5K files, 4.6M relationships
use codanna::io::{
    ExitCode, ProgressBar, ProgressBarOptions, ProgressBarStyle, Spinner, SpinnerOptions,
    status_line::StatusLine,
};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn render_style_preview(style: ProgressBarStyle, ratio: f64, width: usize) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let total = width.max(1);
    let filled = (ratio * total as f64).round() as usize;
    let filled = filled.min(total);
    let empty = total - filled;
    format!(
        "{}{}",
        style.filled_cell().repeat(filled),
        style.empty_cell().repeat(empty)
    )
}

fn simulate_indexing(progress: Arc<ProgressBar>) {
    for _ in 0..8508 {
        let symbols = rand::random::<u64>() % 10;
        progress.inc();
        progress.add_extra1(symbols);
        thread::sleep(Duration::from_micros(200));
    }
    thread::sleep(Duration::from_millis(100));
}

fn simulate_relationships(progress: Arc<ProgressBar>) {
    let total = 100_000u64;
    for i in 0..total {
        progress.inc();
        if i % 100 == 0 {
            progress.add_extra1(1);
        } else {
            progress.add_extra2(1);
        }
        if i % 1000 == 0 {
            thread::sleep(Duration::from_micros(10));
        }
    }
    thread::sleep(Duration::from_millis(100));
}

fn simulate_streaming(spinner: Arc<Spinner>, fail_after: Option<u64>) {
    for i in 0..10_000 {
        if spinner.is_finished() {
            break;
        }

        spinner.tick();
        if i % 10 == 0 {
            spinner.add_extra(1);
        }

        if let Some(target) = fail_after
            && i == target
        {
            spinner.mark_failure(
                ExitCode::BlockingError,
                "Simulated upstream cancellation during streaming",
            );
            break;
        }

        thread::sleep(Duration::from_millis(1));
    }

    if !spinner.is_finished() {
        spinner.mark_success();
    }

    thread::sleep(Duration::from_millis(100));
}

fn main() {
    println!("Simple Progress Demo");
    println!("====================\n");

    println!("Progress Bar Style Preview");
    println!("--------------------------");
    for (style, label) in [
        (ProgressBarStyle::Braille, "Braille (⣿)"),
        (ProgressBarStyle::FullBlock, "Full Block (█)"),
        (ProgressBarStyle::DarkShade, "Dark Shade (▓)"),
        (ProgressBarStyle::MediumShade, "Medium Shade (▒)"),
        (ProgressBarStyle::LightShade, "Light Shade (░)"),
        (ProgressBarStyle::LeftSevenEighths, "Left 7/8 Block (▉)"),
        (ProgressBarStyle::LeftThreeQuarters, "Left 3/4 Block (▊)"),
        (ProgressBarStyle::LeftFiveEighths, "Left 5/8 Block (▋)"),
        (ProgressBarStyle::VerticalSolid, "Vertical Solid (▮)"),
        (ProgressBarStyle::VerticalLight, "Vertical Light (▯)"),
        (
            ProgressBarStyle::ParallelogramSolid,
            "Parallelogram Solid (▰)",
        ),
        (
            ProgressBarStyle::ParallelogramLight,
            "Parallelogram Light (▱)",
        ),
    ] {
        let preview = render_style_preview(style, 0.65, 24);
        println!("  {label:<18}: [{preview}]");
    }
    println!();

    // Phase 1: Indexing
    println!("Phase 1: Indexing Files (Progress Bar)");
    let indexing_options = ProgressBarOptions::default()
        .with_style(ProgressBarStyle::FullBlock)
        .with_width(24);
    let indexing = Arc::new(ProgressBar::with_options(
        8508,
        "files",
        "symbols",
        "",
        indexing_options,
    ));
    let status1 = StatusLine::new(Arc::clone(&indexing));
    let start1 = Instant::now();

    thread::spawn({
        let progress = Arc::clone(&indexing);
        move || simulate_indexing(progress)
    })
    .join()
    .unwrap();

    println!("\n{}", *status1);
    drop(status1);
    println!("Complete in {:.2}s\n", start1.elapsed().as_secs_f64());

    // Phase 2: Relationships
    println!("\nPhase 2: Processing Relationships (Progress Bar)");
    let relationships_options = ProgressBarOptions::default()
        .with_style(ProgressBarStyle::LeftFiveEighths)
        .with_width(24)
        .show_rate(false);
    let rels = Arc::new(ProgressBar::with_options(
        100_000,
        "rels",
        "resolved",
        "skipped",
        relationships_options,
    ));
    let status2 = StatusLine::new(Arc::clone(&rels));
    let start2 = Instant::now();

    thread::spawn({
        let progress = Arc::clone(&rels);
        move || simulate_relationships(progress)
    })
    .join()
    .unwrap();

    println!("\n{}", *status2);
    drop(status2);
    println!("Complete in {:.2}s\n", start2.elapsed().as_secs_f64());

    // Phase 3: Spinner (unknown total)
    println!("\nPhase 3: Streaming Processing (Animated Spinner)");
    let spinner_options = SpinnerOptions::default().with_frame_period(Duration::from_millis(120));
    let spinner = Arc::new(Spinner::with_options(
        "Processing",
        "batches",
        spinner_options,
    ));
    let status3 = StatusLine::new(Arc::clone(&spinner));
    let start3 = Instant::now();

    thread::spawn({
        let spinner = Arc::clone(&spinner);
        move || simulate_streaming(spinner, None)
    })
    .join()
    .unwrap();

    println!("\n{}", *status3);
    drop(status3);
    println!("Complete in {:.2}s\n", start3.elapsed().as_secs_f64());

    // Phase 4: Demonstrate failure handling
    println!("\nPhase 4: Streaming Failure (Animated Spinner with Exit Codes)");
    let failing_spinner_options =
        SpinnerOptions::default().with_frame_period(Duration::from_millis(120));
    let failing_spinner = Arc::new(Spinner::with_options(
        "Resolving",
        "retry batches",
        failing_spinner_options,
    ));
    let status4 = StatusLine::new(Arc::clone(&failing_spinner));
    let start4 = Instant::now();

    thread::spawn({
        let spinner = Arc::clone(&failing_spinner);
        move || simulate_streaming(spinner, Some(2500))
    })
    .join()
    .unwrap();

    println!("\n{}", *status4);
    drop(status4);
    println!(
        "Aborted in {:.2}s (exit code {})\n",
        start4.elapsed().as_secs_f64(),
        failing_spinner.current_exit_code() as u8
    );

    println!("\n====================");
    println!("Demo Complete!");
}
