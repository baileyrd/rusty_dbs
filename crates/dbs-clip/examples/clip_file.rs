//! Clips a saved HTML file and prints the markdown, for eyeballing a
//! page by hand: `cargo run -p dbs-clip --example clip_file -- page.html https://source/url`.

use std::process::ExitCode;

use dbs_clip::{clip, FetchPath, SourceMeta, Url};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path, source] = args.as_slice() else {
        eprintln!("usage: clip_file <page.html> <source-url>");
        return ExitCode::from(2);
    };
    let html = match std::fs::read_to_string(path) {
        Ok(html) => html,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let source = match Url::parse(source) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("bad source url {source}: {e}");
            return ExitCode::from(2);
        }
    };
    let meta = SourceMeta {
        source,
        clipped: chrono::Utc::now().date_naive(),
        fetch_path: FetchPath::Live,
        raindrop_id: None,
        saved: None,
        title: None,
        tags: Vec::new(),
        highlights: Vec::new(),
    };
    match clip(&html, &meta) {
        Ok(clipped) => {
            print!("{}", clipped.markdown());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
