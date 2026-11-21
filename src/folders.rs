use std::path::Path;

use gtk::glib::{self, UserDirectory as Dir};

#[derive(Clone, Copy)]
pub enum FolderIcon {
    Home,
    Root,
    Download,
    Document,
    Desktop,
    Music,
    Picture,
    Video,
    Public,
    Template,
    Folder,
}

impl FolderIcon {
    pub fn for_path(path: &Path) -> Self {
        if path == Path::new("/") {
            return Self::Root;
        }
        if std::env::home_dir().as_deref() == Some(path) {
            return Self::Home;
        }
        [
            (Dir::Downloads, Self::Download),
            (Dir::Documents, Self::Document),
            (Dir::Desktop, Self::Desktop),
            (Dir::Music, Self::Music),
            (Dir::Pictures, Self::Picture),
            (Dir::Videos, Self::Video),
            (Dir::PublicShare, Self::Public),
            (Dir::Templates, Self::Template),
        ]
        .into_iter()
        .find_map(|(id, icon)| {
            (glib::user_special_dir(id).as_deref() == Some(path)).then_some(icon)
        })
        .unwrap_or(Self::Folder)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Home => "user-home-symbolic",
            Self::Root => "drive-harddisk-symbolic",
            Self::Download => "folder-download-symbolic",
            Self::Document => "folder-documents-symbolic",
            Self::Desktop => "user-desktop-symbolic",
            Self::Music => "folder-music-symbolic",
            Self::Picture => "folder-pictures-symbolic",
            Self::Video => "folder-videos-symbolic",
            Self::Public => "folder-publicshare-symbolic",
            Self::Template => "folder-templates-symbolic",
            Self::Folder => "folder-symbolic",
        }
    }

    pub fn draw(self, cr: &gtk::cairo::Context, x: f64, y: f64) {
        let path = |points: &[(f64, f64)]| {
            if let Some((first, rest)) = points.split_first() {
                cr.move_to(x + first.0, y + first.1);
                for &(dx, dy) in rest {
                    cr.line_to(x + dx, y + dy);
                }
            }
        };
        cr.set_line_width(1.2);
        cr.set_line_cap(gtk::cairo::LineCap::Round);
        cr.set_line_join(gtk::cairo::LineJoin::Round);
        match self {
            Self::Home => {
                path(&[(-6., 0.), (0., -5.5), (6., 0.)]);
                path(&[(-4.4, -1.), (-4.4, 5.), (4.4, 5.), (4.4, -1.)]);
                path(&[(-1.4, 5.), (-1.4, 1.7), (1.4, 1.7), (1.4, 5.)]);
            }
            Self::Root => {
                cr.rectangle(x - 6., y - 3.5, 12., 7.);
                path(&[(-3.6, 1.), (-1.8, 1.)]);
                path(&[(2.5, 1.), (3.7, 1.)]);
            }
            Self::Download => {
                path(&[(0., -5.5), (0., 2.)]);
                path(&[(-3., -1.), (0., 2.), (3., -1.)]);
                path(&[(-5., 2.5), (-5., 5.), (5., 5.), (5., 2.5)]);
            }
            Self::Document | Self::Template => {
                path(&[
                    (-4., -5.),
                    (1., -5.),
                    (4., -2.),
                    (4., 5.),
                    (-4., 5.),
                    (-4., -5.),
                ]);
                path(&[(1., -5.), (1., -2.), (4., -2.)]);
                path(&[(-2., 1.), (2., 1.)]);
                path(&[(-2., 3.), (2., 3.)]);
            }
            Self::Desktop => {
                cr.rectangle(x - 6., y - 4., 12., 7.5);
                path(&[(0., 3.5), (0., 5.5)]);
                path(&[(-3., 5.5), (3., 5.5)]);
            }
            Self::Music => {
                path(&[(0., 3.5), (0., -5.), (4.5, -3.)]);
                cr.new_sub_path();
                cr.arc(x - 2., y + 3.5, 2., 0., std::f64::consts::TAU);
            }
            Self::Picture => {
                cr.rectangle(x - 5.5, y - 4.5, 11., 9.);
                path(&[(-4.5, 3.), (-1., -0.3), (1., 1.5), (3., -0.5), (5.5, 2.)]);
            }
            Self::Video => {
                cr.rectangle(x - 5.5, y - 4., 11., 8.);
                path(&[(-1.5, -2.3), (2.5, 0.), (-1.5, 2.3), (-1.5, -2.3)]);
            }
            Self::Public => {
                path(&[(-5., 0.), (4.5, -4.), (-5., 0.), (4.5, 4.)]);
                for (dx, dy) in [(-5., 0.), (4.5, -4.), (4.5, 4.)] {
                    cr.new_sub_path();
                    cr.arc(x + dx, y + dy, 1.35, 0., std::f64::consts::TAU);
                }
            }
            Self::Folder => {
                path(&[
                    (-6., 4.),
                    (-6., -4.),
                    (-1.5, -4.),
                    (0., -2.),
                    (6., -2.),
                    (6., 4.),
                    (-6., 4.),
                ]);
            }
        }
        let _ = cr.stroke();
    }
}
