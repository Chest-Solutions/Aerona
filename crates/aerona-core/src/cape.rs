//! Minecraft capes: where a new one is announced, and how it is drawn.
//!
//! ## What "a cape came out" means here
//!
//! Mojang does not publish a cape list. A cape's texture is not in the client jar and not in the
//! public asset index (checked against 26.3: the index has 5147 objects and the word "cape" appears
//! in none of them, and the jar holds only `PlayerCapeModel.class` and `CapeLayer.class`) - the
//! artwork is delivered per account, through the profile, like a skin. So there is no Mojang endpoint
//! to poll, and pretending otherwise would mean a feature that quietly stops working the day a cape
//! ships.
//!
//! What does exist, and is maintained by people who care, is the wiki: every cape has a page in
//! `Category:Capes`, and each page is the article an operator wants handed to them - what the cape
//! is, and how to get it. So the notifier polls that one category through the MediaWiki API
//! (`action=query`, `generator=categorymembers`, `prop=info`), treats a title it has not seen as a
//! new cape, and takes the texture from the page's own file - the wiki's convention for the flat
//! artwork is `<Cape Name> (texture) JE.png`, a 64x32 PNG - so the same read that finds the news
//! finds the pixels to draw.
//!
//! Three rules follow from that and are enforced in this file, because they are the difference
//! between a notifier and a spam machine: the poll is one request; a cape is announced once per
//! server, ever; and nothing about the announcement depends on the picture. A cape with no image
//! on its page is still news, and it is announced with its name and its article.
//!
//! The picture has two routes, which is what the data on the wiki forces. Three of the sixty cape
//! pages carry the flat `64x32` artwork under a `(texture)` name, and those are draped here, at
//! four angles. The rest carry only the wiki's own render of the cape hanging off a shoulder
//! (280x636) - that is a real render, just not ours, so it is attached as it is rather than
//! invented. Both routes end in the same table, so the panel and the message do not care which one
//! a given cape came from.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The category every cape page belongs to. If the wiki renames it, the poll finds no members and
/// the panel says the source answered with nothing - which is loud, unlike a silent empty list.
pub const CATEGORY: &str = "Category:Capes";
/// The API we ask. `https` and no key: this is a public read of public data.
pub const API_BASE: &str = "https://minecraft.wiki/api.php";
/// Who we are, in the one header Wikimedia asks every client to send. A generic UA is how a
/// well-behaved integration gets blocked without anybody being told why.
pub const USER_AGENT: &str = concat!(
    "Aerona/",
    env!("CARGO_PKG_VERSION"),
    " (Minecraft cape notifier; https://github.com/Aerona/aerona)"
);

/// One page in the category, as the poll learns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapePage {
    /// The page title, verbatim: `15th Anniversary Cape`.
    pub title: String,
    /// The wiki's own id, which is the only stable key available across a rename.
    pub page_id: i64,
    /// `lastrevid`, used as the revision marker: a page whose newest revision nobody has seen is
    /// reported as an update, which is what an operator wants when the article gains a date.
    pub last_rev: i64,
    /// `https://minecraft.wiki/w/15th_Anniversary_Cape` - the article the announcement links to.
    pub url: String,
}

/// A texture file found on a cape's page, with the bytes still to be fetched.
/// One file the wiki says it has for a cape, with enough metadata to decide whether to fetch it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapeImage {
    /// `File:15th Anniversary Cape (texture) JE.png`
    pub file: String,
    /// The download url the API handed back (it carries the wiki's cache-busting suffix).
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub mime: String,
    /// Bytes on the server's side of the file, so a 400 KB screenshot can be refused before it is
    /// downloaded into the database.
    pub size: u64,
}

impl CapeImage {
    /// Whether this file can be draped: a PNG whose shape is the flat artwork - a cape is twice as
    /// wide as it is tall in UV space, at 64x32 or the HD double. The mime test lives here rather
    /// than at the call site because a file that cannot be decoded is not a texture in any sense
    /// that matters, and the renderer would only answer with an error.
    pub fn can_drape(&self) -> bool {
        self.mime.eq_ignore_ascii_case("image/png")
            && self.width >= 32
            && self.height >= 16
            && self.width * 10 >= self.height * 17
            && self.width * 10 <= self.height * 23
    }

    /// True for a picture worth attaching when there is no flat artwork to drape: the wiki's cape
    /// renders are 280x636 portraits of the cape hanging off a shoulder. PNG only, because the
    /// bytes go straight into the frame table and out again as an attachment with no re-encoding.
    /// The aspect test is what keeps a 714x501 in-game screenshot out of a channel announcement,
    /// and the byte cap keeps a blob of any size from creeping into a database that also holds the
    /// moderation log.
    pub fn is_portrait_render(&self) -> bool {
        self.mime.eq_ignore_ascii_case("image/png")
            && self.height >= self.width * 6 / 5
            && self.size <= 400_000
    }
}

/// What the wiki offers for one cape page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapeImages {
    /// The flat texture, if the page documents one. Draped by the renderer in this module.
    pub texture: Option<String>,
    /// The wiki's own picture of the cape, if it has one. Attached as it is.
    pub render: Option<String>,
}

impl CapeImages {
    pub fn is_empty(&self) -> bool {
        self.texture.is_none() && self.render.is_none()
    }
}

/// The poll request. One call, all members, because the category is small and MediaWiki's default
/// limit is 10, which would report the oldest ten capes forever and never the new one.
pub fn query_url() -> String {
    format!(
        "{API_BASE}?action=query&generator=categorymembers&gcmtitle={}&gcmlimit=500&prop=info&inprop=url&format=json",
        url_encode(CATEGORY)
    )
}

/// The second call, and only for a cape that has not been announced: the file's download url and
/// its dimensions. Asking for `url` is what keeps us from assembling `images/<name>` by hand, which
/// is a path the wiki reserves the right to change (files live under a hash subdirectory on
/// commons, and `?<token>` on this one).
pub fn texture_url(files: &[String]) -> String {
    let titles = files
        .iter()
        .map(|f| url_encode(f))
        .collect::<Vec<_>>()
        .join("|");
    format!(
        "{API_BASE}?action=query&prop=imageinfo&iiprop=url%7Csize%7Cmime&format=json&titles={titles}"
    )
}

/// The member list, as far as it is shaped like the API's own document. Anything that is not a page
/// in namespace 0 is skipped rather than guessed at: a category that starts answering with an error
/// object must not become "no capes", which reads as "nothing happened".
///
/// `Err` is the loud answer, for the caller to report.
pub fn parse_category(body: &Value) -> Result<Vec<CapePage>, String> {
    if let Some(err) = body.get("error") {
        return Err(format!(
            "the wiki said: {}",
            err.get("info")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        ));
    }
    let pages = body
        .get("query")
        .and_then(|q| q.get("pages"))
        .and_then(Value::as_object)
        .ok_or_else(|| "the wiki answered without a page list".to_string())?;
    let mut out = Vec::with_capacity(pages.len());
    for page in pages.values() {
        let ns = page.get("ns").and_then(Value::as_i64).unwrap_or(-1);
        let Some(title) = page.get("title").and_then(Value::as_str) else {
            continue;
        };
        // Namespace 0 only: a category page and a disambiguation page are not capes.
        if ns != 0 {
            continue;
        }
        out.push(CapePage {
            title: title.trim().to_string(),
            page_id: page.get("pageid").and_then(Value::as_i64).unwrap_or(0),
            last_rev: page.get("lastrevid").and_then(Value::as_i64).unwrap_or(0),
            url: page
                .get("fullurl")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        });
    }
    out.sort_by(|a, b| b.last_rev.cmp(&a.last_rev).then(a.title.cmp(&b.title)));
    Ok(out)
}

/// Which of a page's files is the cape's artwork, and which is a screenshot of somebody wearing it.
///
/// The pattern is the wiki's own naming: `(texture)` marks the flat file, and `JE` marks the Java
/// Edition one (a `BE` file is the same picture for Bedrock, and rendering both would double every
/// announcement for no news). An `HD` file is skipped: high-resolution capes are a per-account
/// upload, not a release.
///
/// Which file on a cape page is the flat artwork and which is the picture to attach.
///
/// The matching is by name, not by trust: a cape page's image list contains every sprite in the
/// category as well as its own files (the gallery on `15th Anniversary Cape` lists sixty of them),
/// so a candidate that does not carry the page's title is not that cape's image. Names below are
/// the ones the wiki actually uses - `File:<title> (texture) JE.png` and `File:<title> (texture).png`
/// for the artwork, `File:<title>.png`, `File:<title> JE.png` and `File:<title> render.png` for the
/// picture - and the rejects are the ones that would look wrong in an announcement: the elytra
/// (a different model), the icons, the logos and the screenshots.
pub fn cape_images(title: &str, images: &Value) -> CapeImages {
    cape_images_from(title, &listed_files(images))
}

/// The file names a `prop=images` *response* lists for one page, by title.
///
/// A response keys its pages by page id, so handing the whole body to [`listed_files`] finds nothing -
/// which is a silent, plausible-looking empty rather than an error. This is the lookup the callers
/// outside this crate want; `listed_files` is the half that reads one page object.
pub fn page_files(body: &Value, title: &str) -> Vec<String> {
    let Some(pages) = body
        .get("query")
        .and_then(|q| q.get("pages"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    for page in pages.values() {
        if page
            .get("title")
            .and_then(Value::as_str)
            .is_some_and(|t| t.eq_ignore_ascii_case(title))
        {
            return listed_files(page);
        }
    }
    Vec::new()
}

/// The file names a `prop=images` answer lists for one page. Empty is normal: see [`wanted_files`].
pub fn listed_files(images: &Value) -> Vec<String> {
    let Some(list) = images.get("images").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|i| i.get("title").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// The names a cape page's artwork is expected to have, best first.
///
/// These are asked for rather than read off the page, and that is the whole point. Measured against
/// the live category, `prop=images` returns *nothing* for 56 of 60 cape pages, because the wiki's
/// cape files are named by infobox templates rather than transcluded - and on the four pages that do
/// answer, the list is the gallery noise around the article (Turtle Cape offers `File:Open Barrel.png`
/// and `File:BiomeSprite dripstone-cave.png` before anything about turtles). Naming a file and asking
/// whether it exists is what works: the same six names resolve artwork for 43 of the 60 pages,
/// including every recent cape. Both sources are fed to the picker, so a page that documents its
/// files under an odd name is still read.
pub fn wanted_files(title: &str) -> Vec<String> {
    let t = title.trim();
    [
        format!("File:{t} (texture) JE.png"),
        // The java/bedrock split has an odd third naming too: the older capes in the category are
        // `Cod Cape (texture) JE1.png`, with the 1. A list that misses them reads as a cape with no
        // art, which is the failure a moderator notices and the bot does not.
        format!("File:{t} (texture) JE1.png"),
        format!("File:{t} (texture) BE1.png"),
        format!("File:{t} (texture).png"),
        format!("File:{t} (texture) BE.png"),
        format!("File:{t}.png"),
        format!("File:{t} JE.png"),
        format!("File:{t} render.png"),
        format!("File:{t} (cape).png"),
    ]
    .into_iter()
    .collect()
}

/// The subset of a page's own file list that could be *this* cape's file.
///
/// A cape page's `prop=images` answer, when it has one, is mostly the gallery noise around the
/// article: on Turtle Cape it opens with a disambiguation dot, a minecart and an open barrel. The
/// picker would ignore those, since it only accepts a name that carries the page's title - but the
/// names are also what the next request is made of, and the wiki answers at most fifty titles at a
/// time. So the noise is dropped here, before a busy gallery pushes the cape's own names past that
/// limit and turns "no artwork" into a wrong answer to a rejected request.
pub fn about_page(files: &[String], title: &str) -> Vec<String> {
    let t = title.trim().to_ascii_lowercase();
    files
        .iter()
        .filter(|f| f.to_ascii_lowercase().contains(&t))
        .cloned()
        .collect()
}

/// Which of these names is the flat artwork and which is the picture, given the ones that exist.
pub fn cape_images_from(title: &str, files: &[String]) -> CapeImages {
    CapeImages {
        texture: pick_flat(title, files),
        render: pick_picture(title, files),
    }
}

/// The flat texture, best first: the Java edition file named after this page, then any `(texture)`
/// file that is named after it, Java before Bedrock because the Bedrock file is sometimes a crop.
fn pick_flat(title: &str, names: &[String]) -> Option<String> {
    let named = |name: &str, n: &str| n.eq_ignore_ascii_case(name);
    let t = title.trim();
    let exact = format!("File:{t} (texture) JE.png");
    if let Some(hit) = names.iter().find(|n| named(&exact, n)) {
        return Some(hit.clone());
    }
    let plain = format!("File:{t} (texture).png");
    if let Some(hit) = names.iter().find(|n| named(&plain, n)) {
        return Some(hit.clone());
    }
    let soft = format!("File:{t} (texture)");
    names
        .iter()
        .filter(|n| {
            let lower = n.to_ascii_lowercase();
            lower.starts_with(&soft.to_ascii_lowercase())
                && lower.ends_with(".png")
                && !lower.contains("hd")
                && !lower.contains("elytra")
        })
        // `JE` before `BE`, and an unmarked file before either.
        .min_by_key(|n| {
            let lower = n.to_ascii_lowercase();
            (
                if lower.contains(") je") {
                    0
                } else if lower.contains(") be") {
                    2
                } else {
                    1
                },
                lower.len(),
            )
        })
        .cloned()
}

/// The page's own picture of the cape, if the wiki has one under the page's name.
fn pick_picture(title: &str, names: &[String]) -> Option<String> {
    let t = title.trim();
    for suffix in [".png", " JE.png", " render.png", " (cape).png"] {
        let want = format!("File:{t}{suffix}");
        if let Some(hit) = names.iter().find(|n| n.eq_ignore_ascii_case(&want)) {
            if !rejected(naming_of(hit)) {
                return Some(hit.clone());
            }
        }
    }
    None
}

fn naming_of(file: &str) -> String {
    file.to_ascii_lowercase()
}

/// Names that are never the cape's own render, whatever they are attached to.
fn rejected(lower: String) -> bool {
    [
        "texture",
        "elytra",
        "icon",
        "sprite",
        "logo",
        "banner",
        "menu",
        "skin",
        "creator",
        "artwork",
        "screenshot",
        "story",
        "jpeg",
        "jpg",
        "gif",
        "webp",
        "svg",
    ]
    .iter()
    .any(|bad| lower.contains(bad))
}

/// The file list of one cape page, which is a second call and not part of the poll.
///
/// `prop=images` and `generator=categorymembers` do not work together: the API answers the
/// category request happily and simply leaves `images` off every page, which reads as "these capes
/// have no artwork" rather than as a bad query. So the poll asks for pages, and this asks - only for
/// the pages that are new - which files they name.
pub fn images_url(pages: &[String]) -> String {
    let titles = pages
        .iter()
        .map(|p| url_encode(p))
        .collect::<Vec<_>>()
        .join("|");
    format!("{API_BASE}?action=query&prop=images&imlimit=200&format=json&titles={titles}")
}

/// The imageinfo answer for one file: the url to download, if the file exists and is a picture.
/// Every file an `imageinfo` answer says actually exists, in whatever order the API chose.
///
/// A title the wiki does not have comes back as a page with `missing` and no `imageinfo`, which is
/// how a guessed file name is turned down; this is what lets a whole batch of candidates be asked
/// about in one request instead of one request per name.
pub fn image_infos(body: &Value) -> Vec<CapeImage> {
    let Some(pages) = body
        .get("query")
        .and_then(|q| q.get("pages"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(pages.len());
    for page in pages.values() {
        if page.get("missing").is_some() {
            continue;
        }
        let title = page.get("title").and_then(Value::as_str).unwrap_or("");
        let Some(info) = page.get("imageinfo").and_then(Value::as_array) else {
            continue;
        };
        let Some(info) = info.first() else { continue };
        let Some(url) = info.get("url").and_then(Value::as_str) else {
            continue;
        };
        let number = |k: &str| info.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
        out.push(CapeImage {
            file: title.to_string(),
            url: url.to_string(),
            width: number("width"),
            height: number("height"),
            size: info
                .get("size")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| u64::from(number("size"))),
            mime: info
                .get("mime")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
        });
    }
    out
}

pub fn parse_image(body: &Value, want: &str) -> Option<CapeImage> {
    image_infos(body)
        .into_iter()
        .find(|i| i.file.eq_ignore_ascii_case(want))
}

/// `2010 holiday capes` and `MCC 15th Year Cape` are not shapes a button wants. The rule is the one
/// a copy editor would apply by hand: title-case a plain word, leave a word that opens with a digit
/// alone (`15th`, `2010`, `4J`), and leave a word that is already shouting alone (`MCC`), because
/// lowercasing an acronym turns a name into a word.
pub fn display_name(title: &str) -> String {
    title
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(|word| {
            let head = word.chars().next().unwrap_or(' ');
            if head.is_ascii_digit() || (word.len() > 1 && word == word.to_uppercase()) {
                return word.to_string();
            }
            let mut chars = word.chars();
            let first = chars.next().unwrap_or(' ').to_uppercase().to_string();
            first + &chars.as_str().to_lowercase()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The key a cape is filed and deduplicated under: the page id, so a rename cannot make the same
/// cape look new twice.
pub fn slug(page: &CapePage) -> String {
    format!("mw{}", page.page_id)
}

/// Percent-encoding, for the handful of characters a MediaWiki title can contain. `encode_uri`'s
/// component set over-encodes for a query string, and being explicit here means the URL in a log
/// line is the URL that was sent.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The render
// ---------------------------------------------------------------------------
//
// Discord shows a static image, so the "3d render" has to be one we make. What it must be made
// from is not a guess: a cape texture is a 64x32 sheet holding the *unwrap of a box*, and Mojang's
// capes are that and nothing else - the client jar for the current release (34,227 entries) contains
// no cape textures at all, and neither does its asset index, so the layout below came from measuring
// real textures rather than from a model file anybody has to trust.
//
// A cape in the game is a box 10 wide, 16 tall and 1 deep, in units of 4 texels, and the sheet holds
// its unwrap in the top-left corner:
//
//   x  0..1     the left edge            (one texel: the box is one deep)
//   x  1..11    one face
//   x  11..12   the right edge
//   x  12..22   the other face
//   y  0..1     the yoke,  1..17 the faces' height,  17..18 the hem
//
// Everything to the right of x=22 is empty except the shoulder overlay, and every row below 18 is
// empty. A renderer that treats the *sheet* as the picture - which is what the first version of this
// did - stretches 64 columns of mostly-transparent nothing across the shoulders and produces a smudge
// with the side columns hanging off it like a comb. That is the whole difference between the two
// renders a guild has seen.
//
// So: find the box, take the two faces, wrap them around an ellipse for the shoulders, keep the
// pixel art sharp with nearest sampling, and light it from one direction.

/// One face of the cape, in texture pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Face {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Where a sheet's faces are, in its own pixels: `back` and `front` are the two 10x16 panels, the
/// two one-texel columns are their edges, and `yoke`/`hem` are the caps. `scale` is 1 for a 64x32
/// texture and 2 for the 128x64 high-resolution variant some packs ship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sheet {
    pub scale: u32,
    pub back: Face,
    pub front: Face,
    pub left_edge: Face,
    pub right_edge: Face,
}

impl Sheet {
    /// The box, for a sheet of this size. `None` for anything that is not laid out like a cape,
    /// which is the honest answer: a 900x600 screenshot is not a texture, and rendering it as one
    /// is how a notifier ends up posting art that is nobody's cape.
    pub fn of(width: u32, height: u32) -> Option<Sheet> {
        // A cape sheet is twice as wide as it is tall and is at least the vanilla 64x32.
        if width < 64 || height < 32 || width != height * 2 || width % 64 != 0 {
            return None;
        }
        let scale = width / 64;
        let (w, h) = (10 * scale, 16 * scale);
        let y = scale;
        let back = Face {
            x: 12 * scale,
            y,
            w,
            h,
        };
        let front = Face { x: scale, y, w, h };
        Some(Sheet {
            scale,
            back,
            front,
            left_edge: Face {
                x: 0,
                y,
                w: scale,
                h,
            },
            right_edge: Face {
                x: 11 * scale,
                y,
                w: scale,
                h,
            },
        })
    }

    /// Which of the two faces is the one the world sees: the back, because a cape's art is on its
    /// back. "Busier" is measured as the number of different colours in the face, which is a proxy
    /// for "somebody drew something here" and is exactly the thing that is testable. A sheet whose
    /// two faces are identical (a flat colour, a plain migrator cape) makes the choice irrelevant.
    pub fn design_face(&self, rgba: &[u8], width: u32) -> Face {
        let score = |f: &Face| face_variety(rgba, width, f);
        if score(&self.front) > score(&self.back) {
            self.front
        } else {
            self.back
        }
    }
}

/// How many distinct opaque colours a face holds, capped so a photoreal texture costs the same as a
/// pixel-art one.
fn face_variety(rgba: &[u8], width: u32, f: &Face) -> usize {
    let mut seen = std::collections::HashSet::new();
    for y in f.y..f.y + f.h {
        for x in f.x..f.x + f.w {
            let i = ((y * width + x) * 4) as usize;
            let (r, g, b, a) = (
                rgba.get(i).copied().unwrap_or(0),
                rgba.get(i + 1).copied().unwrap_or(0),
                rgba.get(i + 2).copied().unwrap_or(0),
                rgba.get(i + 3).copied().unwrap_or(0),
            );
            if a < 40 {
                continue;
            }
            // Four bits a channel: a gradient should not outscore a design by being noisy.
            seen.insert((r >> 4, g >> 4, b >> 4));
            if seen.len() > 64 {
                return 64;
            }
        }
    }
    seen.len()
}

/// Semi-axes of the ellipse the cape hangs on, in world units: a player's body is 8 wide by 4 deep,
/// and a cape is cut wider than the body it hangs off.
const RX: f32 = 2.65;
const RZ: f32 = 2.05;
/// How far the hem swings forward, so the cape hangs instead of standing.
const LEAN: f32 = 0.62;
/// How deep the folds are, and how many of them.
const FOLD: f32 = 0.085;
const FOLD_WAVES: f32 = 3.0;

/// The drape angles the panel and Discord get, in radians. Two views a little off centre - a cape
/// seen from dead behind is a rectangle, and the fold is the thing worth drawing - and one square on.
pub const YAWS: [f32; 3] = [-0.55, 0.55, 0.0];
/// How many frames one cape has: the drapes, plus the sheet's own art laid flat.
pub const FRAMES: usize = YAWS.len() + 1;
/// The frame an announcement shows. The first drape, because the off-centre view is the one that
/// reads as cloth at the size Discord renders an attachment.
pub const HERO_FRAME: usize = 0;
/// The frame that is the texture itself, unprojected: what the art looks like before anything was
/// done to it, which is the frame a moderator wants when they do not trust the one above it.
pub const FLAT_FRAME: usize = YAWS.len();
/// Output size in pixels. Square, because Discord crops a wide embed image to a fixed aspect and a
/// square one is what survives a mobile layout.
pub const CANVAS: u32 = 320;

/// Where a point of the band hangs, in world units. `col` and `row` are *grid* coordinates, so they
/// run from 0 to the number of columns and rows inclusive: a face's four corners are four of these,
/// which is what lets the renderer draw a surface rather than a scatter of dots. (The first version
/// of this drew one square per texel, sized by guesswork, and the result was a grid of confetti with
/// daylight between the pieces.)
fn band(col: f32, row: f32, cols: f32, rows: f32) -> [f32; 3] {
    let drop = 8.0f32; // 16 texture rows at four texels to a world unit
    let t = (col / cols.max(1.0)).clamp(0.0, 1.0);
    let s = (row / rows.max(1.0)).clamp(0.0, 1.0);
    // A full turn around the shoulders, with the design face centred on the camera: the camera sits
    // behind the player (a cape is not worn on the chest), so the first half of the band, which is
    // the face we chose, has to be the half nearest it. Hence the quarter turn.
    let theta = (t - 0.25) * std::f32::consts::PI * 2.0;
    // A cape is narrower at the hem than at the yoke, and cloth folds. The fold is damped at the
    // yoke because a cape is sewn to a shoulder, not pinned in mid air.
    let taper = 1.0 - 0.11 * s;
    let fold = 1.0 + FOLD * s * (t * std::f32::consts::PI * FOLD_WAVES * 2.0 + s * 1.6).sin();
    let x = RX * theta.sin() * taper * fold;
    let z = -RZ * theta.cos() * taper * fold - LEAN * s * s;
    let y = -drop * s;
    [x, y, z]
}

/// One frame: the cape seen from `yaw`, as RGBA8 of `CANVAS` x `CANVAS`.
///
/// Orthographic projection (a cape on a shoulder is not a perspective study, and an orthographic one
/// keeps every fold the same size, which is what makes the artwork legible at the size Discord shows
/// it), a flat-shaded quad per texture pixel, painted far to near. Texture sampling is nearest
/// because the texture is pixel art, and blurring it is how a 12x12 emblem turns to porridge.
pub fn project(rgba: &[u8], width: u32, height: u32, yaw: f32) -> Vec<u8> {
    let size = (CANVAS * CANVAS) as usize;
    let mut out = vec![0u8; size * 4];
    let Some(sheet) = Sheet::of(width, height) else {
        return out;
    };
    let back = sheet.design_face(rgba, width);
    let front = if back.x == sheet.back.x {
        sheet.front
    } else {
        sheet.back
    };
    let (cols, rows) = ((back.w + front.w).max(2) as f32, back.h.max(1) as f32);
    let texel = |f: &Face, c: u32, r: u32| -> [u8; 4] {
        let x = f.x + c.min(f.w.saturating_sub(1));
        let y = f.y + r.min(f.h.saturating_sub(1));
        let i = ((y * width + x) * 4) as usize;
        [
            rgba.get(i).copied().unwrap_or(0),
            rgba.get(i + 1).copied().unwrap_or(0),
            rgba.get(i + 2).copied().unwrap_or(0),
            rgba.get(i + 3).copied().unwrap_or(0),
        ]
    };

    // The whole grid, projected once. The quads are then corners of this mesh, so the surface cannot
    // have a seam in it however the shape bends.
    let (gx, gy) = (cols as u32 + 1, rows as u32 + 1);
    let mut pts: Vec<[f32; 2]> = Vec::with_capacity((gx * gy) as usize);
    let mut zs: Vec<f32> = Vec::with_capacity((gx * gy) as usize);
    // The same grid before it is turned and flattened, kept because lighting has to be computed on
    // the real surface: a normal from the projected points is a normal of the *picture*, and every
    // quad in it faces the same way, which is how a drape comes out as one flat brightness.
    let mut world: Vec<[f32; 3]> = Vec::with_capacity((gx * gy) as usize);
    let (sy, cy) = yaw.sin_cos();
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for c in 0..gx {
        for r in 0..gy {
            let w = band(c as f32, r as f32, cols, rows);
            world.push(w);
            let x = w[0] * cy + w[2] * sy;
            let z = -w[0] * sy + w[2] * cy;
            let (px, py) = (x, -w[1]);
            min_x = min_x.min(px);
            max_x = max_x.max(px);
            min_y = min_y.min(py);
            max_y = max_y.max(py);
            pts.push([px, py]);
            zs.push(z);
        }
    }
    let span_x = (max_x - min_x).max(1e-3);
    let span_y = (max_y - min_y).max(1e-3);
    let margin = 14.0f32;
    let scale = ((CANVAS as f32 - 2.0 * margin) / span_x.max(span_y)).max(1.0);
    let off_x = (CANVAS as f32 - span_x * scale) * 0.5 - min_x * scale;
    let off_y = (CANVAS as f32 - span_y * scale) * 0.5 - min_y * scale;
    let at = |c: u32, r: u32| -> ([f32; 2], f32) {
        let i = (c * gy + r) as usize;
        (
            [pts[i][0] * scale + off_x, pts[i][1] * scale + off_y],
            zs[i],
        )
    };

    // One light, from the upper left of the camera, with enough ambient that a shadowed fold is a
    // dark cape and not a hole in the picture.
    let light = {
        let l = [-0.42f32, 0.55, -0.72];
        let n = (l[0] * l[0] + l[1] * l[1] + l[2] * l[2]).sqrt();
        [l[0] / n, l[1] / n, l[2] / n]
    };

    struct Quad {
        z: f32,
        xy: [[f32; 2]; 4],
        rgb: [u8; 3],
        a: u8,
    }
    let mut quads: Vec<Quad> = Vec::with_capacity((cols * rows) as usize);
    for c in 0..cols as u32 {
        let (face, local) = if c < back.w {
            (&back, c)
        } else {
            (&front, c - back.w)
        };
        for r in 0..rows as u32 {
            let p = texel(face, local, r);
            if p[3] < 40 {
                continue;
            }
            let (a, az) = at(c, r);
            let (b, bz) = at(c + 1, r);
            let (d, dz) = at(c + 1, r + 1);
            let (e, ez) = at(c, r + 1);
            // The quad's own normal, from its diagonals: flat shading per texel is what makes a fold
            // read as a fold, and it is honest - this *is* a faceted surface, one facet per pixel of
            // cloth.
            let n = face_normal(c, r, gy, &world);
            // Turn the normal with the surface, then one Lambert term against the light.
            let nc = [n[0] * cy + n[2] * sy, n[1], -n[0] * sy + n[2] * cy];
            let dot = (nc[0] * light[0] + nc[1] * light[1] + nc[2] * light[2]).abs();
            let shade = (0.42 + 0.72 * dot).min(1.2);
            quads.push(Quad {
                z: (az + bz + dz + ez) * 0.25,
                xy: [a, b, d, e],
                rgb: [
                    (p[0] as f32 * shade).min(255.0) as u8,
                    (p[1] as f32 * shade).min(255.0) as u8,
                    (p[2] as f32 * shade).min(255.0) as u8,
                ],
                a: p[3],
            });
        }
    }
    // Far to near. The camera looks from behind the player toward +z, so a larger `z` is further
    // away and gets painted first.
    quads.sort_by(|a, b| b.z.partial_cmp(&a.z).unwrap_or(std::cmp::Ordering::Equal));
    for q in quads {
        let (mut x0, mut x1) = (f32::MAX, f32::MIN);
        let (mut y0, mut y1) = (f32::MAX, f32::MIN);
        for p in q.xy {
            x0 = x0.min(p[0]);
            x1 = x1.max(p[0]);
            y0 = y0.min(p[1]);
            y1 = y1.max(p[1]);
        }
        let (ix0, ix1) = (
            x0.floor().max(0.0) as i32,
            x1.ceil().min(CANVAS as f32) as i32,
        );
        let (iy0, iy1) = (
            y0.floor().max(0.0) as i32,
            y1.ceil().min(CANVAS as f32) as i32,
        );
        for py in iy0..iy1 {
            for px in ix0..ix1 {
                if !inside(q.xy, px as f32 + 0.5, py as f32 + 0.5) {
                    continue;
                }
                let o = (py as u32 * CANVAS + px as u32) as usize;
                let t = o * 4;
                // Paint over whatever is there, including a transparent pixel: the quads are already
                // in depth order, and a pixel that has been touched is opaque cloth.
                let _ = t;
                out[o * 4..o * 4 + 4].copy_from_slice(&[q.rgb[0], q.rgb[1], q.rgb[2], q.a]);
            }
        }
    }
    out
}

/// The surface normal of one quad, from the two edges that meet at its first corner.
fn face_normal(c: u32, r: u32, gy: u32, world: &[[f32; 3]]) -> [f32; 3] {
    let p = |cc: u32, rr: u32| world[(cc * gy + rr) as usize];
    let a = p(c, r);
    let b = p(c + 1, r);
    let d = p(c, r + 1);
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [d[0] - a[0], d[1] - a[1], d[2] - a[2]];
    let n = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if len < 1e-9 {
        return [0.0, 0.0, -1.0];
    }
    [n[0] / len, n[1] / len, n[2] / len]
}

/// Is a pixel inside this quad? Half-plane test on the four edges, in screen space. Quads here are
/// convex (they are one texel of a smooth band), so "left of every edge" is the whole answer.
fn inside(quad: [[f32; 2]; 4], x: f32, y: f32) -> bool {
    let mut sign = 0i8;
    for i in 0..4 {
        let [ax, ay] = quad[i];
        let [bx, by] = quad[(i + 1) % 4];
        let cross = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
        let s = if cross > 0.0 {
            1
        } else if cross < 0.0 {
            -1
        } else {
            0
        };
        if s == 0 {
            continue;
        }
        if sign == 0 {
            sign = s;
        } else if sign != s {
            return false;
        }
    }
    true
}

/// The sheet's own art, big: the bounding box of everything the texture actually paints, nearest
/// upscaled into the canvas. No projection, no shading, nothing that could be argued with.
pub fn flat(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let size = (CANVAS * CANVAS) as usize;
    let mut out = vec![0u8; size * 4];
    let opaque = |x: u32, y: u32| -> bool {
        let i = ((y * width + x) * 4) as usize;
        rgba.get(i + 3).copied().unwrap_or(0) >= 40
    };
    let (mut x0, mut y0, mut x1, mut y1) = (width, height, 0u32, 0u32);
    for y in 0..height {
        for x in 0..width {
            if opaque(x, y) {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x1 < x0 || y1 < y0 {
        return out;
    }
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    let margin = 16;
    let scale = ((CANVAS - 2 * margin) / w.max(h)).max(1);
    let (dw, dh) = (w * scale, h * scale);
    let (ox, oy) = ((CANVAS - dw) / 2, (CANVAS - dh) / 2);
    for y in 0..h {
        for x in 0..w {
            let i = (((y0 + y) * width + x0 + x) * 4) as usize;
            let (r, g, b, a) = (
                rgba.get(i).copied().unwrap_or(0),
                rgba.get(i + 1).copied().unwrap_or(0),
                rgba.get(i + 2).copied().unwrap_or(0),
                rgba.get(i + 3).copied().unwrap_or(0),
            );
            if a < 40 {
                continue;
            }
            for dy in 0..scale {
                for dx in 0..scale {
                    let px = ox + x * scale + dx;
                    let py = oy + y * scale + dy;
                    if px >= CANVAS || py >= CANVAS {
                        continue;
                    }
                    let t = ((py * CANVAS + px) as usize) * 4;
                    out[t..t + 4].copy_from_slice(&[r, g, b, a]);
                }
            }
        }
    }
    out
}

/// A cape texture in, the frames out, PNG-encoded.
///
/// `Err` rather than a placeholder on a bad file: the announcement still goes out without a picture,
/// and the row says why, which is better than a rendered smudge of somebody's anniversary artwork.
/// The same `Err` comes back for a texture that is not laid out like a cape, which is the case where
/// guessing would be worst: a wrong box is a wrong picture, and a wrong picture with a cape's name
/// on it is misinformation.
pub fn render_frames(png: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let (w, h, rgba) = decode_rgba(png)?;
    let (w, h) = (w as u32, h as u32);
    if Sheet::of(w, h).is_none() {
        return Err(format!(
            "{w}x{h} is not a cape sheet (a 64x32 unwrap, or a whole multiple of it)"
        ));
    }
    let mut frames = Vec::with_capacity(FRAMES);
    for yaw in YAWS {
        let pixels = project(&rgba, w, h, yaw);
        frames.push(encode_png(&pixels, CANVAS, CANVAS).map_err(|e| e.to_string())?);
    }
    let pixels = flat(&rgba, w, h);
    frames.push(encode_png(&pixels, CANVAS, CANVAS).map_err(|e| e.to_string())?);
    Ok(frames)
}

/// One line about the last pass, for whoever is reading a panel and wondering whether this is
/// working. It is built here rather than in the dashboard because the same sentence is written into
/// the bot's log, and two wordings is two chances to say different things about one event.
pub fn status_line(report: &Value) -> String {
    let data = report.get("data").unwrap_or(report);
    if let Some(error) = data.get("error").and_then(Value::as_str) {
        return format!("the last look failed: {error}");
    }
    let number = |k: &str| data.get(k).and_then(Value::as_u64).unwrap_or(0);
    let pages = number("pages");
    if pages == 0 {
        return "the source has not answered a poll yet".into();
    }
    let mut line = format!("{pages} pages looked at, {} recorded", number("recorded"));
    if number("waiting") > 0 {
        line.push_str(&format!(
            ", {} waiting for the next look",
            number("waiting")
        ));
    }
    if number("stuck") > 0 {
        line.push_str(&format!(
            ", {} not posted after {} tries - check the channel",
            number("stuck"),
            crate::models::CAPE_MAX_ATTEMPTS
        ));
    }
    line
}

/// Decode any 8-bit PNG (truecolor, RGBA, palette or grayscale) into RGBA8.
///
/// `EXPAND` is asked for explicitly so a palette texture from the wiki - which
/// is how every cape artwork on there is stored - arrives as pixels rather than
/// indices, and a `tRNS` chunk becomes a real alpha channel.
fn decode_rgba(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("the cape texture is not a readable PNG: {e}"))?;
    let (w, h) = {
        let info = reader.info();
        if info.bit_depth != png::BitDepth::Eight {
            return Err(format!(
                "cape texture is {:?} bit; Aerona renders 8-bit PNGs",
                info.bit_depth
            ));
        }
        (info.width as usize, info.height as usize)
    };
    if w < 32 || h < 16 {
        return Err(format!("the texture is {w}x{h}, too small to be a cape"));
    }
    let mut buf = vec![0u8; reader.output_buffer_size().max(w * h * 4)];
    let out = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("the cape texture could not be decoded: {e}"))?;
    let rgba = match out.color_type {
        png::ColorType::Rgba => buf[..w * h * 4].to_vec(),
        png::ColorType::Rgb => {
            let src = &buf[..w * h * 3];
            let mut px = Vec::with_capacity(w * h * 4);
            for chunk in src.chunks_exact(3) {
                px.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            px
        }
        png::ColorType::Grayscale => {
            let src = &buf[..w * h];
            let mut px = Vec::with_capacity(w * h * 4);
            for &g in src {
                px.extend_from_slice(&[g, g, g, 255]);
            }
            px
        }
        png::ColorType::GrayscaleAlpha => {
            let src = &buf[..w * h * 2];
            let mut px = Vec::with_capacity(w * h * 4);
            for chunk in src.chunks_exact(2) {
                px.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            px
        }
        other => {
            return Err(format!(
                "cape texture is {other:?}; expected RGBA, RGB or grayscale"
            ))
        }
    };
    Ok((w, h, rgba))
}

/// RGBA8 to PNG, compressed. The `png` crate does the deflate and the chunk CRCs, which is exactly
/// the kind of byte-level detail that should not be hand-rolled in a notifier.
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_line_says_what_the_last_pass_did() {
        assert_eq!(
            status_line(&json!({ "pages": 60, "recorded": 1 })),
            "60 pages looked at, 1 recorded"
        );
        assert_eq!(
            status_line(&json!({ "pages": 60, "recorded": 0, "waiting": 12 })),
            "60 pages looked at, 0 recorded, 12 waiting for the next look"
        );
        assert_eq!(
            status_line(&json!({})),
            "the source has not answered a poll yet",
            "a guild that has never been polled is not a guild with zero capes"
        );
        assert!(
            status_line(&json!({ "error": "the wiki said no" })).contains("the wiki said no"),
            "the failure is the headline when there is one"
        );
        assert!(
            status_line(&json!({ "pages": 6, "stuck": 2 })).contains("not posted after 5 tries"),
            "a shut door is said out loud"
        );
        // The API hands the writer's own envelope to the same function.
        assert_eq!(
            status_line(&json!({ "data": { "pages": 3, "recorded": 2 } })),
            "3 pages looked at, 2 recorded"
        );
    }

    #[test]
    fn a_category_read_becomes_pages_and_a_bad_one_becomes_a_complaint() {
        let good = json!({
            "query": { "pages": {
                "313236": { "pageid": 313236, "ns": 0, "title": "15th Anniversary Cape",
                            "lastrevid": 3827565,
                            "fullurl": "https://minecraft.wiki/w/15th_Anniversary_Cape" },
                "10": { "pageid": 10, "ns": 0, "title": "Cape", "lastrevid": 1,
                        "fullurl": "https://minecraft.wiki/w/Cape" },
                "99": { "pageid": 99, "ns": 14, "title": "Category:Capes", "lastrevid": 2 }
            } }
        });
        let pages = parse_category(&good).expect("a read");
        assert_eq!(
            pages.len(),
            2,
            "the category page itself is not a cape: {pages:?}"
        );
        assert_eq!(
            pages[0].title, "15th Anniversary Cape",
            "newest revision first"
        );
        assert_eq!(slug(&pages[0]), "mw313236");
        assert_eq!(
            pages[0].url, "https://minecraft.wiki/w/15th_Anniversary_Cape",
            "the article link is the wiki's own, not one we spell"
        );

        let err = parse_category(&json!({ "error": { "info": "everything is fine over here" } }));
        assert!(
            err.unwrap_err().contains("everything is fine"),
            "a refusal is passed through, not read as an empty category"
        );
        assert!(parse_category(&json!({ "nothing": 1 })).is_err());
    }

    #[test]
    fn the_flat_file_is_the_one_that_rolls_off_a_shoulder() {
        // These name sets are copied from live cape pages. The common case is not "which texture"
        // but "which of these forty files is this cape", because a page's gallery lists every sprite
        // in the category.
        let page = json!({ "images": [
            {"title": "File:15thCape artwork.jpeg"},
            {"title": "File:15th Anniversary Cape (texture) JE.png"},
            {"title": "File:15th Anniversary Cape JE.png"},
            {"title": "File:15th Anniversary Elytra JE.png"},
            {"title": "File:PersonaSprite 15th-anniversary-cape.png"},
            {"title": "File:Disambig color.svg"}
        ]});
        let got = cape_images("15th Anniversary Cape", &page);
        assert_eq!(
            got.texture.as_deref(),
            Some("File:15th Anniversary Cape (texture) JE.png"),
            "{got:?}"
        );
        assert_eq!(
            got.render.as_deref(),
            Some("File:15th Anniversary Cape JE.png"),
            "the render is the JE file, not the artwork jpeg or the icon"
        );

        // An older page spells neither suffix, and the texture has no edition marker at all.
        let old = json!({ "images": [
            {"title": "File:Bacon Cape.png"},
            {"title": "File:Bacon Cape (texture).png"}
        ]});
        let got = cape_images("Bacon Cape", &old);
        assert_eq!(
            got.texture.as_deref(),
            Some("File:Bacon Cape (texture).png")
        );
        assert_eq!(got.render.as_deref(), Some("File:Bacon Cape.png"));

        // Cobalt names its render `render`; its elytra texture must never be draped as a cape.
        let cobalt = json!({ "images": [
            {"title": "File:Cobalt Cape (texture).png"},
            {"title": "File:Cobalt Cape render.png"},
            {"title": "File:Cobalt Elytra.png"},
            {"title": "File:Cobalt Radwasp.png"}
        ]});
        let got = cape_images("Cobalt Cape", &cobalt);
        assert_eq!(
            got.texture.as_deref(),
            Some("File:Cobalt Cape (texture).png")
        );
        assert_eq!(got.render.as_deref(), Some("File:Cobalt Cape render.png"));

        // A page with only an icon and a photo offers nothing to show. That is a text announcement.
        let bare = json!({ "images": [
            {"title": "File:Invicon Navy Cape.png"},
            {"title": "File:Navy Cape.jpg"}
        ]});
        assert!(
            cape_images("Navy Cape", &bare).is_empty(),
            "no png named for the cape"
        );
        assert!(
            cape_images("Whatever", &json!({})).is_empty(),
            "no images key at all"
        );
    }

    #[test]
    fn a_picture_is_accepted_by_its_shape_not_its_name() {
        let body = json!({ "query": { "pages": { "1": {
            "title": "File:Foo (texture) JE.png",
            "imageinfo": [{ "url": "https://minecraft.wiki/images/Foo.png?abc", "width": 64,
                           "height": 32, "mime": "image/png", "size": 1105 }]
        } } } });
        let flat = parse_image(&body, "File:Foo (texture) JE.png").expect("a file");
        assert_eq!(flat.url, "https://minecraft.wiki/images/Foo.png?abc");
        assert_eq!(flat.size, 1105);
        assert!(flat.can_drape(), "64x32 png is the artwork");
        assert!(
            !flat.is_portrait_render(),
            "and it is not the picture to attach"
        );
        assert!(
            parse_image(&body, "File:Other.png").is_none(),
            "never hand back the wrong cape's file"
        );

        let render = CapeImage {
            file: "File:Bacon Cape.png".into(),
            url: "https://minecraft.wiki/images/Bacon_Cape.png?x".into(),
            width: 280,
            height: 636,
            mime: "image/png".into(),
            size: 12_712,
        };
        assert!(!render.can_drape(), "a portrait is not a texture");
        assert!(
            render.is_portrait_render(),
            "but it is exactly the fallback"
        );

        // A screenshot is neither: the wrong shape and far too big to keep in the database.
        let shot = CapeImage {
            width: 714,
            height: 501,
            size: 438_793,
            ..render.clone()
        };
        assert!(!shot.can_drape(), "714x501 is not cape artwork");
        assert!(
            !shot.is_portrait_render(),
            "no 428K screenshot goes into a message"
        );

        // The HD double of the artwork is the same layout; a crop is not.
        assert!(CapeImage {
            width: 128,
            height: 64,
            ..render.clone()
        }
        .can_drape());
        assert!(!CapeImage {
            width: 64,
            height: 64,
            ..render.clone()
        }
        .can_drape());
        // A jpeg cannot be draped: no alpha, and it would need re-encoding to attach.
        let jpg = CapeImage {
            width: 64,
            height: 32,
            mime: "image/jpeg".into(),
            size: 900,
            ..render
        };
        assert!(
            !jpg.can_drape() && !jpg.is_portrait_render(),
            "a jpeg is neither: {jpg:?}"
        );
    }

    #[test]
    fn a_title_becomes_a_name_that_reads_like_a_thing() {
        assert_eq!(
            display_name("15th Anniversary Cape"),
            "15th Anniversary Cape"
        );
        assert_eq!(display_name("2010 holiday capes"), "2010 Holiday Capes");
        assert_eq!(display_name("MCC 15th Year Cape"), "MCC 15th Year Cape");
        assert_eq!(display_name("follower’s cape"), "Follower’s Cape");
        assert_eq!(display_name("  "), "");
    }

    #[test]
    fn a_url_survives_a_title_with_a_colon_and_a_space() {
        assert_eq!(url_encode("Category:Capes"), "Category%3ACapes");
        assert!(query_url().contains("gcmtitle=Category%3ACapes"));
    }

    #[test]
    fn a_render_is_a_picture_of_the_cape_not_a_copy_of_the_file() {
        // A solid magenta cape with a transparent left edge: the drape must keep the colour, drop
        // the transparent part, and change shape as it turns.
        let w = 64u32;
        let h = 32u32;
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let (r, g, b, a) = if x < 4 {
                    (0, 0, 0, 0)
                } else {
                    (255, 0, 255, 255)
                };
                rgba[i] = r;
                rgba[i + 1] = g;
                rgba[i + 2] = b;
                rgba[i + 3] = a;
            }
        }
        let flat = project(&rgba, w, h, 0.0);
        let lit = project(&rgba, w, h, 0.62);
        let count = |p: &[u8]| (0..p.len()).step_by(4).filter(|i| p[i + 3] > 200).count();
        let (a, b) = (count(&flat), count(&lit));
        assert!(a > 4_000, "a cape fills a good part of the frame: {a} px");
        assert!(
            (a as i64 - b as i64).abs() > 200,
            "turning it changes the silhouette, so this is not a flat paste: {a} vs {b}"
        );
        // Shading: at least two brightnesses across one flat colour is the fold being lit.
        let shades: std::collections::BTreeSet<u8> = (0..flat.len())
            .step_by(4)
            .filter(|i| flat[i + 3] > 200)
            .map(|i| flat[i])
            .collect();
        assert!(
            shades.len() > 3,
            "a drape has more than one brightness in it: {shades:?}"
        );
        assert!(
            flat[..(w * h * 4) as usize / 8]
                .chunks_exact(4)
                .any(|p| p[3] == 0),
            "the canvas starts and stays transparent"
        );

        let png = encode_png(&flat, CANVAS, CANVAS).expect("a png");
        assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        let dec = png::Decoder::new(std::io::Cursor::new(&png))
            .read_info()
            .expect("readable");
        assert_eq!(
            (dec.info().width, dec.info().height),
            (CANVAS, CANVAS),
            "the frames are square at the canvas size"
        );
    }

    #[test]
    fn a_broken_texture_is_said_so_not_drawn_as_garbage() {
        assert!(render_frames(b"not a png").is_err());
        assert!(render_frames(&[]).is_err());
        // A tiny PNG: too small to be a cape, and saying so is the whole point.
        let mut rgba = vec![0u8; 16 * 8 * 4];
        for p in rgba.chunks_exact_mut(4) {
            p.copy_from_slice(&[9, 9, 9, 255]);
        }
        let tiny = encode_png(&rgba, 16, 8).unwrap();
        let e = render_frames(&tiny).unwrap_err();
        assert!(e.contains("too small"), "{e}");
    }

    #[test]
    fn the_frames_are_two_turned_views_a_square_one_and_the_sheet_itself() {
        assert_eq!(YAWS.len(), 3, "{YAWS:?}");
        assert_eq!(FRAMES, 4);
        assert_eq!(
            crate::models::CAPE_FRAMES,
            FRAMES,
            "the count the panel steps through is the count the renderer makes"
        );
        assert!(
            YAWS.iter().all(|y| y.abs() > 1e-3) || YAWS.contains(&0.0),
            "the square-on view is one of them: {YAWS:?}"
        );
        assert!(
            YAWS[HERO_FRAME].abs() > 0.3,
            "the hero is a turned view, because dead behind a cape is a rectangle"
        );
        assert_eq!(FLAT_FRAME, YAWS.len(), "the flat frame is the last one");
    }

    /// The layout is not a taste: it is the unwrap of a 10x16x1 box in a 64x32 sheet, measured off
    /// real textures. A renderer that reads the wrong rect draws somebody's cape as a smudge, so the
    /// numbers are pinned.
    #[test]
    fn the_sheet_is_the_box_mojang_ships() {
        let s = Sheet::of(64, 32).expect("64x32 is the vanilla sheet");
        assert_eq!(
            s.back,
            Face {
                x: 12,
                y: 1,
                w: 10,
                h: 16
            }
        );
        assert_eq!(
            s.front,
            Face {
                x: 1,
                y: 1,
                w: 10,
                h: 16
            }
        );
        assert_eq!(
            s.left_edge,
            Face {
                x: 0,
                y: 1,
                w: 1,
                h: 16
            },
            "the box is one texel deep, so its edge is one column wide"
        );
        let hd = Sheet::of(128, 64).expect("a 2x sheet is the same box");
        assert_eq!(hd.scale, 2);
        assert_eq!(
            hd.back,
            Face {
                x: 24,
                y: 2,
                w: 20,
                h: 32
            }
        );
        // Anything else is not a cape texture, and saying so beats drawing it.
        for (w, h) in [(64, 64), (100, 50), (280, 636), (32, 16), (66, 33)] {
            assert!(Sheet::of(w, h).is_none(), "{w}x{h} is not a cape sheet");
        }
    }

    /// A cape's art is on its back. Which of the two faces that is cannot be read off the sheet -
    /// Mojang gives no label - so it is measured: the busier half is the design, and it is the half
    /// the hero frame turns toward the camera.
    #[test]
    fn the_design_face_is_the_busy_one() {
        let (w, h) = (64u32, 32u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let mut put = |x: u32, y: u32, c: [u8; 4]| {
            let i = ((y * w + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&c);
        };
        // The back face (x 12..22) one flat colour; the front face (x 1..11) a striped design.
        for y in 1..17 {
            for x in 12..22 {
                put(x, y, [200, 30, 30, 255]);
            }
            for x in 1..11 {
                let c = if x % 2 == 0 {
                    [10, 200, 20, 255]
                } else {
                    [250, 250, 250, 255]
                };
                put(x, y, c);
            }
        }
        let s = Sheet::of(w, h).unwrap();
        assert_eq!(
            s.design_face(&rgba, w),
            s.front,
            "the striped half is the one somebody drew"
        );
        // And the flat frame shows *that* sheet, not an empty area of it: enough opaque pixels to
        // fill the canvas at the crop's scale.
        let flat_png = flat(&rgba, w, h);
        let lit = (0..flat_png.len() / 4)
            .filter(|i| flat_png[i * 4 + 3] > 200)
            .count();
        assert!(
            lit > 8_000,
            "the flat frame is a picture, not a speck: {lit} px of {} possible",
            (CANVAS * CANVAS) as usize
        );
        let teal = (0..flat_png.len() / 4)
            .filter(|i| flat_png[i * 4 + 3] > 200 && flat_png[i * 4 + 1] > 150)
            .count();
        assert!(
            teal > 1_000,
            "both faces are in the crop, since a sheet is more than its back: {teal}"
        );
    }

    /// A sheet the renderer cannot read is refused by the frame builder, in words, and the row keeps
    /// the announcement without a picture.
    #[test]
    fn a_square_texture_is_not_draped() {
        let (w, h) = (64u32, 64u32);
        let rgba = vec![255u8; (w * h * 4) as usize];
        let png = encode_png(&rgba, w, h).unwrap();
        let e = render_frames(&png).unwrap_err();
        assert!(
            e.contains("64x64") && e.contains("not a cape sheet"),
            "the error names the size so an operator can see what arrived: {e}"
        );
    }

    // --- the wire, as measured on the live category on 2026-10-26: 60 pages, of which 56 answer
    // `prop=images` with nothing at all, and 43 that do have a file under a name the naming implies.

    #[test]
    fn the_names_worth_asking_for_are_the_ones_the_wiki_uses() {
        let want = wanted_files("Aurora Cape");
        assert_eq!(want.len(), 9, "{want:?}");
        assert_eq!(want[0], "File:Aurora Cape (texture) JE.png");
        assert!(
            want.contains(&"File:Aurora Cape (texture) JE1.png".to_string()),
            "the JE1 naming is how the older capes are filed: {want:?}"
        );
        assert_eq!(want[3], "File:Aurora Cape (texture).png");
        for name in [
            "File:Aurora Cape.png",
            "File:Aurora Cape JE.png",
            "File:Aurora Cape render.png",
            "File:Aurora Cape (cape).png",
        ] {
            assert!(
                want.iter().any(|n| n == name),
                "{name} missing from {want:?}"
            );
        }
        assert_eq!(wanted_files("  Pan Cape "), wanted_files("Pan Cape"));
    }

    #[test]
    fn a_response_is_looked_up_by_title_because_pages_are_keyed_by_id() {
        let body = serde_json::json!({ "query": { "pages": {
            "427556": { "pageid": 427556, "ns": 0, "title": "Aurora Cape", "images": [
                { "ns": 6, "title": "File:Aurora Cape (texture).png" },
                { "ns": 6, "title": "File:BiomeSprite dripstone-cave.png" },
            ] },
            "-1": { "title": "Nowhere Cape", "missing": "" },
        } } });
        assert_eq!(
            page_files(&body, "Aurora Cape"),
            vec![
                "File:Aurora Cape (texture).png".to_string(),
                "File:BiomeSprite dripstone-cave.png".to_string()
            ]
        );
        assert!(
            page_files(&body, "Nowhere Cape").is_empty(),
            "a missing page has no files"
        );
        assert!(page_files(&body, "Turtle Cape").is_empty());
        // The picker drops the sprite that is not about this cape, and keeps the texture.
        let kept = about_page(&page_files(&body, "Aurora Cape"), "Aurora Cape");
        assert_eq!(kept, vec!["File:Aurora Cape (texture).png".to_string()]);
        assert_eq!(
            cape_images("Aurora Cape", &body["query"]["pages"]["427556"]).texture,
            Some("File:Aurora Cape (texture).png".to_string()),
            "the texture on the page's own list is still preferred over a guessed name"
        );
    }

    #[test]
    fn imageinfo_answers_are_read_whole_and_missing_files_are_dropped() {
        let body = serde_json::json!({ "query": { "pages": {
            "1": { "pageid": 1, "title": "File:Aurora Cape (texture).png", "imageinfo": [
                { "url": "https://minecraft.wiki/images/7/7a/Aurora_Cape_%28texture%29.png?ab12",
                  "width": 64, "height": 32, "mime": "image/png", "size": 1041 },
            ] },
            "2": { "pageid": 2, "title": "File:Aurora Cape JE.png", "imageinfo": [
                { "url": "https://minecraft.wiki/images/3/9/Aurora_Cape_JE.png?cd34",
                  "width": 280, "height": 636, "mime": "image/png", "size": 11_340 },
            ] },
            "3": { "pageid": -1, "title": "File:Aurora Cape (texture) JE.png", "missing": "" },
        } } });
        let infos = image_infos(&body);
        assert_eq!(infos.len(), 2, "the missing name is not a file");
        let names: Vec<String> = infos.iter().map(|i| i.file.clone()).collect();
        let picks = cape_images_from("Aurora Cape", &names);
        assert_eq!(
            picks.texture.as_deref(),
            Some("File:Aurora Cape (texture).png")
        );
        assert_eq!(picks.render.as_deref(), Some("File:Aurora Cape JE.png"));
        assert!(infos[0].can_drape(), "64x32 is the flat artwork");
        assert!(!infos[1].can_drape());
        assert!(
            infos[1].is_portrait_render(),
            "280x636 is the picture to attach"
        );
        // The one-file lookup reads the same list, so the two cannot drift.
        assert_eq!(
            parse_image(&body, "file:aurora cape je.png").map(|i| i.size),
            Some(11_340),
            "titles are matched the way MediaWiki returns them, case aside"
        );
        assert!(parse_image(&body, "File:Aurora Cape (texture) JE.png").is_none());
    }

    #[test]
    fn a_page_with_no_list_still_gets_a_picture() {
        // The common case, and the one that made this file's first version useless: an empty
        // `prop=images` answer, artwork found only by asking for the names the naming implies.
        let empty = serde_json::json!({ "query": { "pages": { "9": { "pageid": 9, "title": "Twisted Cape" } } } });
        assert!(page_files(&empty, "Twisted Cape").is_empty());
        let names = wanted_files("Twisted Cape");
        assert_eq!(
            cape_images_from("Twisted Cape", &names)
                .texture
                .as_deref()
                .unwrap(),
            "File:Twisted Cape (texture) JE.png",
            "best first, whether or not it exists: the caller filters by the file list"
        );
    }

    #[test]
    fn an_elytra_is_never_mistaken_for_the_cape_itself() {
        let names = vec!["File:Blueprint Cape Elytra JE.png".to_string()];
        assert!(cape_images_from("Blueprint Cape", &names).is_empty());
    }
}
