//! A live poke at the cape source, for the one thing a fixture cannot prove: that the request this
//! program sends is the request minecraft.wiki answers, and that a file it names as a texture is a
//! texture the renderer can drape.
//!
//! It is `#[ignore]`d, so `cargo test` never depends on somebody else's uptime. Run it by hand when
//! the notifier goes quiet and you want to know whether that is us or them:
//!
//! ```sh
//! cargo test -p aerona-bot --test live_cape_wire -- --ignored --nocapture
//! ```
//!
//! Frames are written to `$TMPDIR/aerona-cape-wire/<Cape-Name>.png` so they can be opened: the hero
//! frame of each draped cape, and the wiki's own picture where that is what was used.
//!
//! The parse and render halves are unit-tested against recorded bodies; what this adds is the wire.

use aerona_core::cape as src;
use serde_json::Value;

async fn get(url: &str) -> Vec<u8> {
    let client = reqwest::Client::builder()
        .user_agent(src::USER_AGENT)
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .expect("a client");
    let res = client.get(url).send().await.expect("the request failed");
    let status = res.status();
    let bytes = res.bytes().await.expect("the body failed");
    assert!(status.is_success(), "{url} answered {status}");
    bytes.to_vec()
}

async fn json(url: &str) -> Value {
    let body: Value = serde_json::from_slice(&get(url).await)
        .unwrap_or_else(|e| panic!("{url} was not json: {e}"));
    if let Some(err) = body.get("error") {
        panic!(
            "{url} answered an error: {}",
            err["info"].as_str().unwrap_or("?")
        );
    }
    body
}

#[tokio::test]
#[ignore = "needs the network: it asks minecraft.wiki for the cape category"]
async fn the_category_answer_parses_and_real_artwork_renders() {
    let body = json(&src::query_url()).await;
    let pages = src::parse_category(&body).expect("the category parsed");
    println!(
        "{} capes in the category; newest revision first: {}",
        pages.len(),
        pages[0].title
    );
    assert!(
        pages.len() > 40,
        "the category has dozens of capes, not none"
    );
    for page in &pages {
        assert!(page.page_id > 0, "{} has no page id", page.title);
        assert!(
            page.url.starts_with("https://minecraft.wiki/"),
            "{} has a strange url",
            page.title
        );
    }

    // The newest ten, through the same three calls the worker makes: what the page lists, what the
    // naming implies, and whether those files exist. Mirroring the product here is the point - a test
    // that reaches around the picker and reads the API its own way proves nothing about the picker.
    // The renders land in the temp dir, not in the repo: whoever runs this wants to open a picture,
    // and a tree that grows output of its own tests is a tree that ships them by accident.
    let out = std::env::temp_dir().join("aerona-cape-wire");
    std::fs::create_dir_all(&out).expect("temp dir");

    let mut draped = 0;
    let mut portrait = 0;
    let mut bare = 0;
    for page in pages.iter().take(10) {
        let listed = src::about_page(
            &src::page_files(
                &json(&src::images_url(std::slice::from_ref(&page.title))).await,
                &page.title,
            ),
            &page.title,
        );
        let mut names = listed.clone();
        for want in src::wanted_files(&page.title) {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&want)) {
                names.push(want);
            }
        }
        let mut infos = Vec::new();
        for chunk in names.chunks(40) {
            infos.extend(src::image_infos(&json(&src::texture_url(chunk)).await));
        }
        let have: Vec<String> = infos.iter().map(|i| i.file.clone()).collect();
        let picks = src::cape_images_from(&page.title, &have);
        let found = |name: &str| {
            infos
                .iter()
                .find(|i| i.file.eq_ignore_ascii_case(name))
                .cloned()
        };
        let name = page.title.replace(' ', "-");
        if let Some(img) = picks
            .texture
            .as_deref()
            .and_then(found)
            .filter(|i| i.can_drape())
        {
            let bytes = get(&img.url).await;
            let frames = src::render_frames(&bytes).unwrap_or_else(|e| {
                panic!(
                    "{}: a texture the picker chose did not drape: {e}",
                    page.title
                )
            });
            assert_eq!(frames.len(), src::FRAMES);
            std::fs::write(out.join(format!("{name}.png")), &frames[src::HERO_FRAME])
                .expect("hero frame");
            std::fs::write(out.join(format!("{name}-all.png")), &frames[0]).ok();
            draped += 1;
            println!(
                "draped {} from {} ({}x{}, listed {} of {} names)",
                page.title,
                img.file,
                img.width,
                img.height,
                listed.len(),
                names.len()
            );
        } else if let Some(img) = picks
            .render
            .as_deref()
            .and_then(found)
            .filter(|i| i.is_portrait_render())
        {
            let bytes = get(&img.url).await;
            std::fs::write(out.join(format!("{name}.png")), &bytes).expect("the render");
            portrait += 1;
            println!("kept the wiki's picture for {} ({})", page.title, img.file);
        } else {
            bare += 1;
            println!(
                "nothing usable for {} (asked {}, {} existed: {})",
                page.title,
                names.len(),
                infos.len(),
                have.iter().take(4).cloned().collect::<Vec<_>>().join(", ")
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }

    println!("{draped} draped, {portrait} portrait-only, {bare} with nothing usable");
    assert!(
        draped + portrait > 0,
        "not one of the ten newest cape pages had a picture this program can attach"
    );
    // The claim that matters for the announcement: a cape with no artwork is still a cape. Nothing
    // above panics on a page with no files, and `CapeImages::is_empty` is what the worker branches on.
    assert!(src::CapeImages::default().is_empty());
}
