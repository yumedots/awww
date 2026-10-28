use common::log::error;
use smallvec::SmallVec;
use waybackend::{Waybackend, objman::ObjectManager};

use rustix::time::Timespec;

use common::{
    compression::Decompressor,
    ipc::{self, BgImg, ImgReq, Nanos, PixelFormat},
    mmap::MmappedBytes,
};

use crate::{WaylandObject, wallpaper::WallpaperCell};

mod keyframe;
mod transitions;
use transitions::Effect;

pub struct Animator {
    pub wallpapers: SmallVec<[WallpaperCell; 2]>,
    now: Timespec,
    start: Timespec,
    half: Timespec,
    released: bool,
    animator: AnimatorKind,
}

enum AnimatorKind {
    Transition(Transition),
    Animation(Animation),
}

impl Animator {
    pub fn new(
        mut wallpapers: SmallVec<[WallpaperCell; 2]>,
        transition: &ipc::Transition,
        img_req: ImgReq,
        animation: Option<ipc::Animation>,
        pixel_format: PixelFormat,
    ) -> Option<Self> {
        let ImgReq {
            img,
            path,
            dim,
            band,
            ..
        } = img_req;
        if wallpapers.is_empty() {
            return None;
        }
        for w in &mut wallpapers {
            let mut w = w.borrow_mut();
            w.set_img_info(BgImg::Img(path.str().into()));
            w.set_band(band);
        }

        let expect = wallpapers[0].borrow().get_dimensions();
        if dim != expect {
            error!(
                "image has wrong dimensions! Expect {}x{}, actual {}x{}",
                expect.0, expect.1, dim.0, dim.1
            );
            return None;
        }
        let effect = Some(Box::new(Effect::new(transition, dim)));
        for w in &mut wallpapers {
            w.borrow_mut().prepare_ink(img.bytes(), pixel_format);
        }
        let start = crate::clock::get();
        Some(Self {
            wallpapers,
            now: start,
            start,
            half: half_of(transition.duration),
            released: false,
            animator: AnimatorKind::Transition(Transition {
                effect,
                fps_nanos: Nanos::from_nanos(1_000_000_000 / transition.fps as u64),
                img,
                animation,
            }),
        })
    }

    pub fn time_to_draw(&self) -> Timespec {
        match &self.animator {
            AnimatorKind::Transition(transition) => transition.time_to_draw(&self.now),
            AnimatorKind::Animation(animation) => animation.time_to_draw(&self.now),
        }
    }

    pub fn updt_time(&mut self) {
        self.now = crate::clock::get();
    }

    /// true once the wall has drawn its first blended frame, which is the frame a bar should
    /// take the new ink. An effect that knows how far it has drawn says so by content, because
    /// the compositor can hold the first frames back and the wall clock would then run ahead of
    /// the picture. An effect with no blend of its own falls back to one frame of its duration.
    fn past_midpoint(&self, transition: &Transition) -> bool {
        if let Some(progress) = transition.effect.as_ref().and_then(|effect| effect.progress()) {
            return progress > 0.0;
        }

        let elapsed = crate::clock::get() - self.start;
        nanos(elapsed) >= nanos(self.half) / 2
    }

    pub fn frame(
        &mut self,
        backend: &mut Waybackend,
        objman: &mut ObjectManager<WaylandObject>,
        pixel_format: PixelFormat,
    ) -> bool {
        if !self.released {
            let past = match &self.animator {
                AnimatorKind::Transition(transition) => self.past_midpoint(transition),
                AnimatorKind::Animation(_) => false,
            };

            if past {
                self.released = true;
                for w in self.wallpapers.iter() {
                    w.borrow_mut().release_ink();
                }
            }
        }

        let Self {
            wallpapers,
            animator,
            ..
        } = self;
        match animator {
            AnimatorKind::Transition(transition) => {
                if !transition.frame(backend, objman, wallpapers.as_mut_slice(), pixel_format) {
                    return false;
                }
                for w in wallpapers.iter() {
                    w.borrow_mut().end_ink(pixel_format);
                }
                // Note: it needs to have more than a single frame, otherwise there is no point in
                // animating it
                if let Some(animation) = transition.animation.take()
                    && animation.animation.len() > 1
                {
                    *animator = AnimatorKind::Animation(Animation {
                        animation,
                        decompressor: Decompressor::new(),
                        i: 0,
                    });
                    return false;
                }
                true
            }
            AnimatorKind::Animation(animation) => {
                animation.frame(backend, objman, wallpapers, pixel_format);
                false
            }
        }
    }
}

struct Transition {
    fps_nanos: Nanos,
    effect: Option<Box<Effect>>,
    img: MmappedBytes,
    animation: Option<ipc::Animation>,
}

impl Transition {
    fn time_to_draw(&self, start: &Timespec) -> Timespec {
        let now = crate::clock::get();
        let elapsed = now - *start;
        timespec_saturating_sub(self.fps_nanos.into_timespec(), elapsed)
    }

    fn frame(
        &mut self,
        backend: &mut Waybackend,
        objman: &mut ObjectManager<WaylandObject>,
        wallpapers: &mut [WallpaperCell],
        pixel_format: PixelFormat,
    ) -> bool {
        let Self { effect, img, .. } = self;
        match effect.as_mut() {
            Some(e) => {
                let over = e.execute(backend, objman, pixel_format, wallpapers, img.bytes());
                if over {
                    *effect = None;
                }
                false
            }
            None => true,
        }
    }
}

struct Animation {
    animation: ipc::Animation,
    decompressor: Decompressor,
    i: usize,
}

impl Animation {
    fn time_to_draw(&self, start: &Timespec) -> Timespec {
        let now = crate::clock::get();
        let elapsed = now - *start;
        timespec_saturating_sub(
            self.animation.animation[self.i % self.animation.animation.len()]
                .1
                .into_timespec(),
            elapsed,
        )
    }

    fn frame(
        &mut self,
        backend: &mut Waybackend,
        objman: &mut ObjectManager<WaylandObject>,
        wallpapers: &mut SmallVec<[WallpaperCell; 2]>,
        pixel_format: PixelFormat,
    ) {
        let Self {
            animation,
            decompressor,
            i,
            ..
        } = self;

        let frame = &animation.animation[*i % animation.animation.len()].0;

        if *i < animation.animation.len() {
            wallpapers.retain(|w| {
                let mut borrow = w.borrow_mut();
                let result = borrow.canvas_change(backend, objman, pixel_format, |canvas| {
                    decompressor.decompress(frame, canvas, pixel_format)
                });
                match result {
                    Ok(()) => true,
                    Err(e) => {
                        error!("failed to unpack frame: {e}");
                        false
                    }
                }
            });
        } else {
            // if we already went through one loop, we can use the unsafe version, because
            // everything was already validated
            for w in wallpapers {
                let mut borrow = w.borrow_mut();
                // SAFETY: we have already validated every frame and removed the ones that have
                // errors in the previous loops. The only ones left should be those that can be
                // decompressed correctly
                borrow.canvas_change(backend, objman, pixel_format, |canvas| unsafe {
                    decompressor.decompress_unchecked(frame, canvas, pixel_format);
                });
            }
        }

        *i += 1;
    }
}

fn nanos(t: Timespec) -> i128 {
    t.tv_sec as i128 * 1_000_000_000 + t.tv_nsec as i128
}

/// `duration` is in seconds, and the wallpaper is half switched halfway through it
fn half_of(duration: f32) -> Timespec {
    let half = (duration as f64 / 2.0 * 1_000_000_000.0) as i64;
    Timespec {
        tv_sec: half / 1_000_000_000,
        tv_nsec: half % 1_000_000_000,
    }
}

/// inspired by the std Duration implementation
fn timespec_saturating_sub(a: Timespec, b: Timespec) -> Timespec {
    let mut res = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    if a.tv_sec >= b.tv_sec {
        let mut secs = a.tv_sec - b.tv_sec;
        let nanos = if a.tv_nsec >= b.tv_nsec {
            a.tv_nsec - b.tv_nsec
        } else if secs > 0 {
            secs -= 1;
            a.tv_nsec + 1_000_000_000 - b.tv_nsec
        } else {
            return res;
        };
        res.tv_sec = secs;
        res.tv_nsec = nanos;
    }

    res
}
