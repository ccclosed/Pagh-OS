//! Backend вывода: headless (PNG/PPM) сейчас, fb0/DRM — позже.
//!
//! Композитор ничего не знает о том, куда выводится кадр: он передаёт непрозрачный
//! холст и список повреждённых прямоугольников. Headless-backend складывает кадры в
//! PNG для тестов и демонстраций; backend `/dev/fb0` (mmap) реализуется отдельно,
//! когда kernel-ветка предоставит fbdev-ABI.

use crate::canvas::Canvas;
use crate::geometry::Rect;
use crate::png;
use std::io;
use std::path::{Path, PathBuf};

/// Приёмник готовых кадров.
pub trait Backend {
    fn size(&self) -> (u32, u32);

    /// Показать кадр; `damage` — перерисованные области (backend может копировать
    /// только их, если у него есть сохранённый предыдущий буфер).
    fn present(&mut self, canvas: &Canvas, damage: &[Rect]) -> io::Result<()>;
}

/// Headless-вывод: считает кадры и (опционально) пишет каждый кадр в PNG.
#[derive(Clone, Debug)]
pub struct HeadlessBackend {
    width: u32,
    height: u32,
    out_dir: Option<PathBuf>,
    max_files: Option<u64>,
    pub frames: u64,
    pub last_damage: Vec<Rect>,
    pub last_path: Option<PathBuf>,
}

impl HeadlessBackend {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            out_dir: None,
            max_files: None,
            frames: 0,
            last_damage: Vec::new(),
            last_path: None,
        }
    }

    pub fn with_screenshot_dir(width: u32, height: u32, dir: impl Into<PathBuf>) -> Self {
        Self {
            out_dir: Some(dir.into()),
            ..Self::new(width, height)
        }
    }

    pub fn set_screenshot_dir(&mut self, dir: impl Into<PathBuf>) {
        self.out_dir = Some(dir.into());
    }

    pub fn set_max_files(&mut self, max: Option<u64>) {
        self.max_files = Some(max.unwrap_or(u64::MAX));
    }

    /// Явно записать текущий кадр (команда `screenshot`).
    pub fn screenshot(&self, path: impl AsRef<Path>, canvas: &Canvas) -> io::Result<()> {
        png::write_png(path.as_ref(), canvas)
    }

    fn frame_path(&self) -> Option<PathBuf> {
        let dir = self.out_dir.as_ref()?;
        if let Some(max) = self.max_files {
            if self.frames > max {
                return None;
            }
        }
        Some(dir.join(format!("frame_{:05}.png", self.frames)))
    }
}

impl Backend for HeadlessBackend {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn present(&mut self, canvas: &Canvas, damage: &[Rect]) -> io::Result<()> {
        self.frames += 1;
        self.last_damage = damage.to_vec();
        if let Some(path) = self.frame_path() {
            self.screenshot(&path, canvas)?;
            self.last_path = Some(path);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::rgb;

    #[test]
    fn headless_counts_frames_and_writes_png() {
        let dir = std::env::temp_dir().join(format!("pagh-backend-{}", std::process::id()));
        let mut backend = HeadlessBackend::with_screenshot_dir(8, 6, &dir);
        let canvas = Canvas::new(8, 6, rgb(1, 2, 3));
        backend.present(&canvas, &[Rect::new(0, 0, 8, 6)]).unwrap();
        assert_eq!(backend.frames, 1);
        let path = backend.last_path.clone().unwrap();
        assert!(path.exists());
        assert!(std::fs::read(&path).unwrap().starts_with(b"\x89PNG"));
        std::fs::remove_dir_all(dir).ok();
    }
}
