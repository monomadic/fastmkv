//! The example in the README, compiled and run, so it cannot drift.

mod common;
use common::*;
use fastmkv::ebml;

#[test]
fn the_readme_example_runs() -> fastmkv::Result<()> {
    let l = seek_head_len(1);
    let parts = [
        seek_head(&[(ebml::INFO, l + 200)]),
        ebml::void(200).unwrap(),
        info(),
        cluster(),
    ];
    let film = write("readme", &file(&parts));
    let copy = write("readme-copy", &file(&parts));
    let seated = temp("readme-seated");

    let m = fastmkv::read(&film.0)?;
    assert_eq!((m.title.as_deref(), m.get("ARTIST")), (None, None));

    let mut mkv = fastmkv::open(&film.0)?;
    mkv.set_title(Some("A title"))?;
    mkv.set("ARTIST", "Someone")?;
    mkv.remove("COMMENT")?;
    let plan = mkv.plan()?;
    plan.apply_to(&copy.0)?;
    let m = fastmkv::read(&copy.0)?;
    assert_eq!(m.title.as_deref(), Some("A title"));

    let mkv = fastmkv::open(&copy.0)?;
    if !mkv.seating().front {
        mkv.reseat(&seated.0, fastmkv::Padding::default())?;
        assert_eq!(fastmkv::open(&seated.0)?.get("ARTIST"), Some("Someone"));
    }
    Ok(())
}
