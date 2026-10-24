//! Modern color spaces (the foundation for `color.space()` / `color.to-space()` /
//! `lab()` / `oklch()` …).
//!
//! Only this file owns the space math: every function takes and returns
//! **normalized channels** (`[f64; 3]`), with reference white points and matrices as
//! defined by CSS Color 4 (`lab`/`lch` are D50, `oklab`/`oklch` and `xyz` are D65).
//! Serialization and the Sass-side unit conversion live in `value.rs` / `modules.rs`.
//!
//! Matrix constants are copied verbatim from CSS Color 4 (trailing digits are not
//! hand-tweaked), and the lints related to them are disabled.

#![allow(clippy::excessive_precision)]

/// Color spaces of CSS Color 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Space {
    /// legacy: `rgb()` / hexadecimal, channels 0..255 (outward) -- internally still
    /// normalized to 0..1.
    Rgb,
    /// legacy: `hsl()`.
    Hsl,
    /// legacy: `hwb()`.
    Hwb,
    Srgb,
    SrgbLinear,
    DisplayP3,
    A98Rgb,
    ProphotoRgb,
    Rec2020,
    XyzD50,
    XyzD65,
    Lab,
    Lch,
    Oklab,
    Oklch,
}

impl Space {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "rgb" => Self::Rgb,
            "hsl" => Self::Hsl,
            "hwb" => Self::Hwb,
            "srgb" => Self::Srgb,
            "srgb-linear" => Self::SrgbLinear,
            "display-p3" => Self::DisplayP3,
            "a98-rgb" => Self::A98Rgb,
            "prophoto-rgb" => Self::ProphotoRgb,
            "rec2020" => Self::Rec2020,
            "xyz" | "xyz-d65" => Self::XyzD65,
            "xyz-d50" => Self::XyzD50,
            "lab" => Self::Lab,
            "lch" => Self::Lch,
            "oklab" => Self::Oklab,
            "oklch" => Self::Oklch,
            _ => return None,
        })
    }

    /// The value returned by `color.space()` (`xyz-d65` normalized to `xyz`).
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Rgb => "rgb",
            Self::Hsl => "hsl",
            Self::Hwb => "hwb",
            Self::Srgb => "srgb",
            Self::SrgbLinear => "srgb-linear",
            Self::DisplayP3 => "display-p3",
            Self::A98Rgb => "a98-rgb",
            Self::ProphotoRgb => "prophoto-rgb",
            Self::Rec2020 => "rec2020",
            Self::XyzD50 => "xyz-d50",
            Self::XyzD65 => "xyz",
            Self::Lab => "lab",
            Self::Lch => "lch",
            Self::Oklab => "oklab",
            Self::Oklch => "oklch",
        }
    }

    /// Only `rgb` / `hsl` / `hwb` are legacy spaces (dart's `isLegacy`).
    pub(crate) fn is_legacy(self) -> bool {
        matches!(self, Self::Rgb | Self::Hsl | Self::Hwb)
    }

    /// Channel names of this space (excluding `alpha`), in serialization order.
    pub(crate) fn channel_names(self) -> [&'static str; 3] {
        match self {
            Self::Rgb
            | Self::Srgb
            | Self::SrgbLinear
            | Self::DisplayP3
            | Self::A98Rgb
            | Self::ProphotoRgb
            | Self::Rec2020 => ["red", "green", "blue"],
            Self::Hsl => ["hue", "saturation", "lightness"],
            Self::Hwb => ["hue", "whiteness", "blackness"],
            Self::XyzD50 | Self::XyzD65 => ["x", "y", "z"],
            Self::Lab | Self::Oklab => ["lightness", "a", "b"],
            Self::Lch | Self::Oklch => ["lightness", "chroma", "hue"],
        }
    }

    /// Channel name -> index; supports dart's shorthands (`red`/`r`, `green`/`g`,
    /// `blue`/`b` …).
    pub(crate) fn channel_index(self, name: &str) -> Option<usize> {
        let lower = name.to_ascii_lowercase();
        let names = self.channel_names();
        if let Some(i) = names.iter().position(|n| *n == lower) {
            return Some(i);
        }
        let aliases: [&[&str]; 3] = match self {
            Self::Rgb
            | Self::Srgb
            | Self::SrgbLinear
            | Self::DisplayP3
            | Self::A98Rgb
            | Self::ProphotoRgb
            | Self::Rec2020 => [&["r"], &["g"], &["b"]],
            Self::Hsl => [&["h"], &["saturation", "sat", "s"], &["lightness", "l"]],
            Self::Hwb => [&["h"], &["whiteness", "w"], &["blackness", "b"]],
            Self::XyzD50 | Self::XyzD65 => [&[], &[], &[]],
            Self::Lab | Self::Oklab => [&["lightness", "l"], &[], &[]],
            Self::Lch | Self::Oklch => [&["lightness", "l"], &["chroma", "c"], &["h"]],
        };
        aliases
            .iter()
            .position(|list| list.contains(&lower.as_str()))
    }

    /// Whether the channel takes a `%` unit (lightness of `lab`, saturation of
    /// `hsl`…).
    pub(crate) fn channel_unit(self, index: usize) -> Option<&'static str> {
        match (self, index) {
            (Self::Hsl | Self::Hwb, 0) => Some("deg"),
            (Self::Hsl | Self::Hwb, 1..=2) => Some("%"),
            (Self::Lab | Self::Oklab, 0) => Some("%"),
            (Self::Lch | Self::Oklch, 0) => Some("%"),
            (Self::Lch | Self::Oklch, 2) => Some("deg"),
            _ => None,
        }
    }

    /// Legal range of the channel (used by `is-in-gamut` and `clip`); `None` =
    /// unbounded.
    pub(crate) fn channel_bounds(self, index: usize) -> Option<(f64, f64)> {
        match self {
            Self::Rgb => Some((0.0, 255.0)),
            Self::Srgb
            | Self::SrgbLinear
            | Self::DisplayP3
            | Self::A98Rgb
            | Self::ProphotoRgb
            | Self::Rec2020 => Some((0.0, 1.0)),
            Self::Hsl => match index {
                0 => Some((0.0, 360.0)),
                1 | 2 => Some((0.0, 100.0)),
                _ => None,
            },
            Self::Hwb => match index {
                0 => Some((0.0, 360.0)),
                _ => Some((0.0, 100.0)),
            },
            Self::Lab => match index {
                0 => Some((0.0, 100.0)),
                1 | 2 => Some((-125.0, 125.0)),
                _ => None,
            },
            Self::Lch => match index {
                0 => Some((0.0, 100.0)),
                1 => Some((0.0, 150.0)),
                _ => None,
            },
            Self::Oklab => match index {
                0 => Some((0.0, 100.0)),
                1 | 2 => Some((-0.4, 0.4)),
                _ => None,
            },
            Self::Oklch => match index {
                0 => Some((0.0, 100.0)),
                1 => Some((0.0, 0.4)),
                _ => None,
            },
            Self::XyzD50 | Self::XyzD65 => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Basic math
// ---------------------------------------------------------------------------

fn mul(m: &[[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// sRGB transfer function (IEC 61966-2-1).
fn srgb_to_linear(c: f64) -> f64 {
    let abs = c.abs();
    if abs <= 0.04045 {
        c / 12.92
    } else {
        c.signum() * ((abs + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f64) -> f64 {
    let abs = c.abs();
    if abs <= 0.0031308 {
        c * 12.92
    } else {
        c.signum() * (1.055 * abs.powf(1.0 / 2.4) - 0.055)
    }
}

const SRGB_TO_XYZ65: [[f64; 3]; 3] = [
    [0.41239079926595934, 0.357584339383878, 0.1804807884018343],
    [0.21263900587151027, 0.715168678767756, 0.07219231536073371],
    [0.01933081871559182, 0.11919477979462598, 0.9505321522496607],
];

const XYZ65_TO_SRGB: [[f64; 3]; 3] = [
    [3.2409699419045226, -1.537383177570094, -0.4986107602930034],
    [-0.9692436362808796, 1.8759675015077202, 0.04155505740717559],
    [
        0.05563007969699366,
        -0.20397695888897652,
        1.0569715142428786,
    ],
];

const DISPLAY_P3_TO_XYZ65: [[f64; 3]; 3] = [
    [0.4865709486482162, 0.26566769316909306, 0.1982172852343625],
    [0.2289745640697488, 0.6917385218365064, 0.079286914093745],
    [0.0, 0.04511338185890264, 1.043944368900976],
];

const XYZ65_TO_DISPLAY_P3: [[f64; 3]; 3] = [
    [2.493496911941425, -0.9313836179191239, -0.40271078445071684],
    [
        -0.8294889695615747,
        1.7626640603183463,
        0.023624685841943577,
    ],
    [
        0.03584583024378447,
        -0.07617238926804182,
        0.9568845240076872,
    ],
];

const A98_TO_XYZ65: [[f64; 3]; 3] = [
    [0.5766690429101305, 0.1855582379065463, 0.1882286462349947],
    [0.29734497525053605, 0.6273635662554661, 0.07529145849399788],
    [0.02703136138641234, 0.07068885253582723, 0.9913375368376388],
];

const XYZ65_TO_A98: [[f64; 3]; 3] = [
    [
        2.0415879038107465,
        -0.5650069742788596,
        -0.34473135077832956,
    ],
    [-0.9692436362808795, 1.8759675015077202, 0.04155505740717557],
    [
        0.013444280632031142,
        -0.11836239223101838,
        1.0151749943912054,
    ],
];

const PROPHOTO_TO_XYZ50: [[f64; 3]; 3] = [
    [0.7977666449006423, 0.13518129740053308, 0.0313477341283922],
    [0.2880748288194013, 0.711835234241873, 0.00008993693872564],
    [0.0, 0.0, 0.8251046025104601],
];

const XYZ50_TO_PROPHOTO: [[f64; 3]; 3] = [
    [
        1.3457868816471583,
        -0.25557208737979464,
        -0.05110186497554526,
    ],
    [-0.5446307051249019, 1.5082477428451468, 0.02052744743642139],
    [0.0, 0.0, 1.2119675456389454],
];

const REC2020_TO_XYZ65: [[f64; 3]; 3] = [
    [0.6369580483012914, 0.14461690358620832, 0.1688809751641721],
    [0.2627002120112671, 0.6779980715188708, 0.05930171646986196],
    [0.0, 0.028072693049087428, 1.060985057710791],
];

const XYZ65_TO_REC2020: [[f64; 3]; 3] = [
    [
        1.716651187971268,
        -0.35567078377639233,
        -0.25336628137365974,
    ],
    [-0.6666843518324892, 1.6164812366349395, 0.01576854581391113],
    [
        0.017639857445310783,
        -0.042770613257808524,
        0.9421031212354738,
    ],
];

/// Bradford chromatic adaptation (D65 <-> D50).
const BRADFORD: [[f64; 3]; 3] = [
    [
        1.0479297925449969,
        0.022946870601609652,
        -0.05019226628920524,
    ],
    [
        0.02962780877005599,
        0.9904344267538799,
        -0.017073799063418826,
    ],
    [
        -0.009243058152591178,
        0.015055144896577895,
        0.7518742814281371,
    ],
];

const BRADFORD_INV: [[f64; 3]; 3] = [
    [0.955473421488075, -0.02309845494876471, 0.06325924320057072],
    [
        -0.0283697093338637,
        1.0099953980813041,
        0.021041441191917323,
    ],
    [
        0.012314014864481998,
        -0.020507649298898964,
        1.330365926242124,
    ],
];

const D50: [f64; 3] = [0.3457 / 0.3585, 1.0, (1.0 - 0.3457 - 0.3585) / 0.3585];

fn a98_to_linear(c: f64) -> f64 {
    c.signum() * c.abs().powf(563.0 / 256.0)
}

fn linear_to_a98(c: f64) -> f64 {
    c.signum() * c.abs().powf(256.0 / 563.0)
}

fn prophoto_to_linear(c: f64) -> f64 {
    let abs = c.abs();
    if abs <= 16.0 / 512.0 {
        c / 16.0
    } else {
        c.signum() * abs.powf(1.8)
    }
}

fn linear_to_prophoto(c: f64) -> f64 {
    let abs = c.abs();
    if abs >= 1.0 / 512.0 {
        c.signum() * abs.powf(1.0 / 1.8)
    } else {
        c * 16.0
    }
}

fn rec2020_to_linear(c: f64) -> f64 {
    const ALPHA: f64 = 1.09929682680944;
    const BETA: f64 = 0.018053968510807;
    let abs = c.abs();
    if abs < BETA * 4.5 {
        c / 4.5
    } else {
        c.signum() * ((abs + ALPHA - 1.0) / ALPHA).powf(1.0 / 0.45)
    }
}

fn linear_to_rec2020(c: f64) -> f64 {
    const ALPHA: f64 = 1.09929682680944;
    const BETA: f64 = 0.018053968510807;
    let abs = c.abs();
    if abs > BETA {
        c.signum() * (ALPHA * abs.powf(0.45) - (ALPHA - 1.0))
    } else {
        c * 4.5
    }
}

fn xyz65_to_xyzd50(xyz: [f64; 3]) -> [f64; 3] {
    mul(&BRADFORD, xyz)
}

fn xyz50_to_xyz65(xyz: [f64; 3]) -> [f64; 3] {
    mul(&BRADFORD_INV, xyz)
}

fn xyz50_to_lab(xyz: [f64; 3]) -> [f64; 3] {
    const EPSILON: f64 = 216.0 / 24389.0;
    const KAPPA: f64 = 24389.0 / 27.0;
    let f = |v: f64, white: f64| {
        let r = v / white;
        if r > EPSILON {
            r.cbrt()
        } else {
            (KAPPA * r + 16.0) / 116.0
        }
    };
    let fx = f(xyz[0], D50[0]);
    let fy = f(xyz[1], D50[1]);
    let fz = f(xyz[2], D50[2]);
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

fn lab_to_xyz50(lab: [f64; 3]) -> [f64; 3] {
    const EPSILON: f64 = 216.0 / 24389.0;
    const KAPPA: f64 = 24389.0 / 27.0;
    let fy = (lab[0] + 16.0) / 116.0;
    let fx = lab[1] / 500.0 + fy;
    let fz = fy - lab[2] / 200.0;
    let finv = |v: f64| {
        let cube = v * v * v;
        if cube > EPSILON {
            cube
        } else {
            (116.0 * v - 16.0) / KAPPA
        }
    };
    [
        finv(fx) * D50[0],
        if lab[0] > KAPPA * EPSILON {
            fy * fy * fy
        } else {
            lab[0] / KAPPA
        },
        finv(fz) * D50[2],
    ]
}

fn xyz65_to_oklab(xyz: [f64; 3]) -> [f64; 3] {
    const M1: [[f64; 3]; 3] = [
        [0.8190224379967030, 0.3619062600528904, -0.1288737815209879],
        [0.0329836539323885, 0.9292868615863434, 0.0361446663506424],
        [0.0481771893596242, 0.2642395317527308, 0.6335478284694309],
    ];
    const M2: [[f64; 3]; 3] = [
        [0.2104542683093140, 0.7936177747023054, -0.0040720430116193],
        [1.9779985324311684, -2.4285922420485799, 0.4505937096174110],
        [0.0259040424655478, 0.7827717124575296, -0.8086757549230774],
    ];
    let lms = mul(&M1, xyz);
    let lms = [lms[0].cbrt(), lms[1].cbrt(), lms[2].cbrt()];
    mul(&M2, lms)
}

fn oklab_to_xyz65(lab: [f64; 3]) -> [f64; 3] {
    const M1: [[f64; 3]; 3] = [
        [1.2268798758459243, -0.5578149944602171, 0.2813910456659647],
        [-0.0405757452148008, 1.1122868032803170, -0.0717110580655164],
        [-0.0763729366746601, -0.4214933324022432, 1.5869240198367816],
    ];
    const M2: [[f64; 3]; 3] = [
        [1.0, 0.3963377773761749, 0.2158037573099136],
        [1.0, -0.1055613458156586, -0.0638541728258133],
        [1.0, -0.0894841775298119, -1.2914855480194092],
    ];
    let lms = mul(&M2, lab);
    let lms = [
        lms[0] * lms[0] * lms[0],
        lms[1] * lms[1] * lms[1],
        lms[2] * lms[2] * lms[2],
    ];
    mul(&M1, lms)
}

fn lab_to_lch(lab: [f64; 3]) -> [f64; 3] {
    let c = (lab[1] * lab[1] + lab[2] * lab[2]).sqrt();
    let mut h = lab[2].atan2(lab[1]).to_degrees();
    if h < 0.0 {
        h += 360.0;
    }
    if h.is_nan() {
        h = 0.0;
    }
    [lab[0], c, h]
}

fn lch_to_lab(lch: [f64; 3]) -> [f64; 3] {
    let h = lch[2].to_radians();
    [lch[0], lch[1] * h.cos(), lch[1] * h.sin()]
}

pub(crate) fn hsl_to_srgb(hsl: [f64; 3]) -> [f64; 3] {
    let (h, s, l) = (hsl[0].rem_euclid(360.0), hsl[1] / 100.0, hsl[2] / 100.0);
    let h = h / 360.0;
    if s == 0.0 {
        return [l, l, l];
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let channel = |mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    [channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0)]
}

/// sRGB -> HSL (`h` in degrees, `s`/`l` 0..1).
pub(crate) fn srgb_to_hsl(rgb: [f64; 3]) -> [f64; 3] {
    let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let delta = max - min;
    if delta == 0.0 {
        return [0.0, 0.0, l];
    }
    let s = if l < 0.5 {
        delta / (max + min)
    } else {
        delta / (2.0 - max - min)
    };
    let h = if max == r {
        ((g - b) / delta).rem_euclid(6.0) * 60.0
    } else if max == g {
        ((b - r) / delta + 2.0) * 60.0
    } else {
        ((r - g) / delta + 4.0) * 60.0
    };
    [h, s, l]
}

/// sRGB -> HWB (`w`/`b` 0..1).
pub(crate) fn srgb_to_hwb(rgb: [f64; 3]) -> [f64; 3] {
    let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
    let white = r.min(g).min(b);
    let black = 1.0 - r.max(g).max(b);
    let hue = srgb_to_hsl(rgb)[0];
    [hue, white, black]
}

/// HWB -> sRGB (`w`/`b` percentages; normalized per CSS when `w + b > 100`).
pub(crate) fn hwb_to_srgb(hwb: [f64; 3]) -> [f64; 3] {
    let (mut white, mut black) = (hwb[1] / 100.0, hwb[2] / 100.0);
    if white + black > 1.0 {
        let sum = white + black;
        white /= sum;
        black /= sum;
    }
    let pure = hsl_to_srgb([hwb[0], 100.0, 50.0]);
    let scale = 1.0 - white - black;
    [
        pure[0] * scale + white,
        pure[1] * scale + white,
        pure[2] * scale + white,
    ]
}

// ---------------------------------------------------------------------------
// Exposed: space conversion
// ---------------------------------------------------------------------------

/// Any space -> normalized sRGB (0..1, may be out of gamut).
pub(crate) fn to_srgb(space: Space, ch: [f64; 3]) -> [f64; 3] {
    match space {
        Space::Rgb | Space::Srgb => ch,
        Space::Hsl => hsl_to_srgb(ch),
        Space::Hwb => hwb_to_srgb(ch),
        Space::SrgbLinear => ch.map(linear_to_srgb),
        Space::DisplayP3 => {
            let xyz = mul(&DISPLAY_P3_TO_XYZ65, ch.map(srgb_to_linear));
            mul(&XYZ65_TO_SRGB, xyz).map(linear_to_srgb)
        }
        Space::A98Rgb => {
            let xyz = mul(&A98_TO_XYZ65, ch.map(a98_to_linear));
            mul(&XYZ65_TO_SRGB, xyz).map(linear_to_srgb)
        }
        Space::ProphotoRgb => {
            let xyz50 = mul(&PROPHOTO_TO_XYZ50, ch.map(prophoto_to_linear));
            let xyz = xyz50_to_xyz65(xyz50);
            mul(&XYZ65_TO_SRGB, xyz).map(linear_to_srgb)
        }
        Space::Rec2020 => {
            let xyz = mul(&REC2020_TO_XYZ65, ch.map(rec2020_to_linear));
            mul(&XYZ65_TO_SRGB, xyz).map(linear_to_srgb)
        }
        Space::XyzD65 => mul(&XYZ65_TO_SRGB, ch).map(linear_to_srgb),
        Space::XyzD50 => mul(&XYZ65_TO_SRGB, xyz50_to_xyz65(ch)).map(linear_to_srgb),
        Space::Lab => {
            let xyz = xyz50_to_xyz65(lab_to_xyz50(ch));
            mul(&XYZ65_TO_SRGB, xyz).map(linear_to_srgb)
        }
        Space::Lch => {
            let xyz = xyz50_to_xyz65(lab_to_xyz50(lch_to_lab(ch)));
            mul(&XYZ65_TO_SRGB, xyz).map(linear_to_srgb)
        }
        Space::Oklab => {
            let xyz = oklab_to_xyz65(ch);
            mul(&XYZ65_TO_SRGB, xyz).map(linear_to_srgb)
        }
        Space::Oklch => {
            let xyz = oklab_to_xyz65(lch_to_lab(ch));
            mul(&XYZ65_TO_SRGB, xyz).map(linear_to_srgb)
        }
    }
}

/// Normalized sRGB -> any space (the `ch` convention is described on `Space`).
pub(crate) fn from_srgb(space: Space, rgb: [f64; 3]) -> [f64; 3] {
    match space {
        Space::Rgb | Space::Srgb => rgb,
        Space::Hsl => {
            let hsl = srgb_to_hsl(rgb);
            [hsl[0], hsl[1] * 100.0, hsl[2] * 100.0]
        }
        Space::Hwb => {
            let hwb = srgb_to_hwb(rgb);
            [hwb[0], hwb[1] * 100.0, hwb[2] * 100.0]
        }
        Space::SrgbLinear => rgb.map(srgb_to_linear),
        Space::DisplayP3 => {
            let xyz = mul(&SRGB_TO_XYZ65, rgb.map(srgb_to_linear));
            mul(&XYZ65_TO_DISPLAY_P3, xyz).map(linear_to_srgb)
        }
        Space::A98Rgb => {
            let xyz = mul(&SRGB_TO_XYZ65, rgb.map(srgb_to_linear));
            mul(&XYZ65_TO_A98, xyz).map(linear_to_a98)
        }
        Space::ProphotoRgb => {
            let xyz = mul(&SRGB_TO_XYZ65, rgb.map(srgb_to_linear));
            let xyz50 = xyz65_to_xyzd50(xyz);
            mul(&XYZ50_TO_PROPHOTO, xyz50).map(linear_to_prophoto)
        }
        Space::Rec2020 => {
            let xyz = mul(&SRGB_TO_XYZ65, rgb.map(srgb_to_linear));
            mul(&XYZ65_TO_REC2020, xyz).map(linear_to_rec2020)
        }
        Space::XyzD65 => mul(&SRGB_TO_XYZ65, rgb.map(srgb_to_linear)),
        Space::XyzD50 => {
            let xyz = mul(&SRGB_TO_XYZ65, rgb.map(srgb_to_linear));
            xyz65_to_xyzd50(xyz)
        }
        Space::Lab => {
            let xyz = mul(&SRGB_TO_XYZ65, rgb.map(srgb_to_linear));
            xyz50_to_lab(xyz65_to_xyzd50(xyz))
        }
        Space::Lch => {
            let xyz = mul(&SRGB_TO_XYZ65, rgb.map(srgb_to_linear));
            lab_to_lch(xyz50_to_lab(xyz65_to_xyzd50(xyz)))
        }
        Space::Oklab => {
            let xyz = mul(&SRGB_TO_XYZ65, rgb.map(srgb_to_linear));
            xyz65_to_oklab(xyz)
        }
        Space::Oklch => {
            let xyz = mul(&SRGB_TO_XYZ65, rgb.map(srgb_to_linear));
            lab_to_lch(xyz65_to_oklab(xyz))
        }
    }
}

/// Direct conversion between spaces (same value-domain convention: `ch` already
/// holds channel values of that space).
pub(crate) fn convert(from: Space, to: Space, ch: [f64; 3]) -> [f64; 3] {
    if from == to {
        return ch;
    }
    from_srgb(to, to_srgb(from, ch))
}

/// Whether both spaces are "bounded at the same time" to sRGB: out-of-gamut is
/// decided by the 0..1 range of normalized sRGB.
pub(crate) fn in_srgb_gamut(rgb: [f64; 3]) -> bool {
    const EPSILON: f64 = 1e-9;
    rgb.iter().all(|c| *c >= -EPSILON && *c <= 1.0 + EPSILON)
}

/// `to-gamut(..., $method: clip)`: clamp within sRGB, then return to the original
/// space.
pub(crate) fn clip_to_gamut(space: Space, ch: [f64; 3]) -> [f64; 3] {
    let srgb = to_srgb(space, ch);
    let clipped = srgb.map(|c| c.clamp(0.0, 1.0));
    from_srgb(space, clipped)
}

/// OKLab color difference (deltaE-OK of CSS Color 4).
pub(crate) fn delta_eok(a: [f64; 3], b: [f64; 3]) -> f64 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// `local-MINDE` gamut mapping of CSS Color 4: bisect chroma down in OKLCH until
/// the deltaE-OK between the clipped and the original color falls below the JND.
pub(crate) fn local_minde_to_gamut(space: Space, ch: [f64; 3]) -> [f64; 3] {
    const JND: f64 = 0.02;
    const EPSILON: f64 = 0.0001;
    let srgb = to_srgb(space, ch);
    if in_srgb_gamut(srgb) {
        return ch;
    }
    let oklch = from_srgb(Space::Oklch, srgb);
    let mut min = 0.0;
    let mut max = oklch[1];
    let mut current = oklch[1];
    let mut clipped = [oklch[0], current, oklch[2]];
    loop {
        let clipped_srgb = to_srgb(Space::Oklch, clipped).map(|c| c.clamp(0.0, 1.0));
        let e = delta_eok(
            from_srgb(Space::Oklab, clipped_srgb),
            from_srgb(Space::Oklab, oklch),
        );
        if e < JND {
            if JND - e < EPSILON {
                break;
            }
            min = current;
        } else {
            max = current;
        }
        current = (min + max) / 2.0;
        if (max - min).abs() < EPSILON {
            break;
        }
        clipped[1] = current;
    }
    let mapped = [oklch[0], current, oklch[2]];
    convert(Space::Oklch, space, mapped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f64; 3], b: [f64; 3], tol: f64) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() <= tol)
    }

    /// Expected values come from dart-sass 1.93.2 (the output of
    /// `color.to-space()`).
    #[test]
    fn conversions_match_modern_dart() {
        // #003366
        let srgb = [0.0, 0.2, 0.4];
        assert!(close(
            from_srgb(Space::Hsl, srgb),
            [210.0, 100.0, 20.0],
            1e-9
        ));
        // The trailing-digit difference (<1e-6) comes from version differences in
        // the matrix constants and is semantically irrelevant.
        assert!(close(
            from_srgb(Space::Lab, srgb),
            [20.7457453073, 1.8876487232, -34.9880898979],
            2e-6
        ));
        assert!(close(
            from_srgb(Space::Oklab, srgb),
            [0.32333781389, -0.0284623813, -0.0985146011],
            1e-8
        ));
        assert!(close(
            from_srgb(Space::Oklch, srgb),
            [0.32333781389, 0.1025438139, 253.8851819964],
            1e-7
        ));
        assert!(close(
            from_srgb(Space::DisplayP3, srgb),
            [0.0690923275, 0.196438359, 0.3861624224],
            2e-6
        ));
        // lab(50 20 30) -> sRGB 0..1
        assert!(close(
            to_srgb(Space::Lab, [50.0, 20.0, 30.0]),
            [0.6301122046, 0.4125440309, 0.2688994203],
            2e-6
        ));
    }

    #[test]
    fn hsl_and_hwb_round_trip() {
        let rgb = [0.25, 0.5, 0.75];
        let hsl = from_srgb(Space::Hsl, rgb);
        assert!(close(to_srgb(Space::Hsl, hsl), rgb, 1e-12));
        let hwb = from_srgb(Space::Hwb, rgb);
        assert!(close(to_srgb(Space::Hwb, hwb), rgb, 1e-12));
    }

    #[test]
    fn gamut_mapping() {
        let oklch = [0.7, 0.4, 30.0];
        assert!(!in_srgb_gamut(to_srgb(Space::Oklch, oklch)));
        let clipped = clip_to_gamut(Space::Oklch, oklch);
        assert!(in_srgb_gamut(to_srgb(Space::Oklch, clipped)));
        // dart 1.93.2: `color.to-gamut(oklch(0.7 0.4 30), $method: clip)` =>
        // `oklch(62.7955363921% 0.2576833038 29.2338802796deg)`
        assert!(close(
            clipped,
            [0.6279553639, 0.2576833038, 29.2338802796],
            1e-7
        ));
    }
}
