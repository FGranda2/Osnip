//! Save a pin's current pixels to disk as PNG.
//!
//! Path resolution: `<config.save_dir>/<expanded_filename>`. The
//! template's `{timestamp}` token is substituted with local time
//! formatted `YYYYMMDD-HHMMSS`. The directory is created on demand.
//! Encoding runs on `spawn_blocking` because PNG encoding is CPU-bound
//! and would otherwise stall the tokio runtime for large images.

use crate::config::Config;
use anyhow::Context;
use chrono::Local;
use image::RgbaImage;
use std::path::PathBuf;
use std::sync::Arc;

/// Write `image` to a fresh PNG inside `cfg.save_dir`. Returns the
/// final path on success.
pub async fn save_pin(image: Arc<RgbaImage>, cfg: Arc<Config>) -> anyhow::Result<PathBuf> {
    let filename = render_template(&cfg.filename_template, &Local::now());
    let path = cfg.save_dir.join(filename);

    tokio::fs::create_dir_all(&cfg.save_dir)
        .await
        .with_context(|| format!("create_dir_all {}", cfg.save_dir.display()))?;

    let path_for_blocking = path.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        image
            .save_with_format(&path_for_blocking, image::ImageFormat::Png)
            .with_context(|| format!("encode PNG to {}", path_for_blocking.display()))
    })
    .await
    .context("save_pin join")??;

    Ok(path)
}

fn render_template(template: &str, now: &chrono::DateTime<Local>) -> String {
    let stamp = now.format("%Y%m%d-%H%M%S").to_string();
    template.replace("{timestamp}", &stamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use image::Rgba;

    fn one_px() -> Arc<RgbaImage> {
        Arc::new(RgbaImage::from_pixel(1, 1, Rgba([200, 100, 50, 255])))
    }

    #[test]
    fn template_substitutes_timestamp() {
        let dt = Local.with_ymd_and_hms(2026, 5, 6, 15, 30, 12).unwrap();
        assert_eq!(
            render_template("osnip-{timestamp}.png", &dt),
            "osnip-20260506-153012.png"
        );
    }

    #[test]
    fn template_without_token_is_passthrough() {
        let dt = Local.with_ymd_and_hms(2026, 5, 6, 15, 30, 12).unwrap();
        assert_eq!(render_template("static.png", &dt), "static.png");
    }

    #[tokio::test]
    async fn writes_a_real_png() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(Config {
            save_dir: dir.path().to_path_buf(),
            filename_template: "test-{timestamp}.png".to_string(),
        });
        let path = save_pin(one_px(), cfg).await.expect("save");
        assert!(path.starts_with(dir.path()));
        let bytes = tokio::fs::read(&path).await.expect("read back");
        // PNG magic bytes.
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[tokio::test]
    async fn creates_missing_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a").join("b");
        let cfg = Arc::new(Config {
            save_dir: nested.clone(),
            filename_template: "x.png".to_string(),
        });
        let path = save_pin(one_px(), cfg).await.expect("save");
        assert!(path.exists());
        assert!(nested.exists());
    }
}
