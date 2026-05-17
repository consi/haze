//! `Slot` - the seven percentile fields for one period.

/// Number of f32 percentile fields per slot. Matches `Slot`'s field count
/// and the on-disk `hzc` chunk column count.
pub const NUM_FIELDS: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub min: f32,
    pub p2_5: f32,
    pub p25: f32,
    pub median: f32,
    pub p75: f32,
    pub p97_5: f32,
    pub loss_pct: f32,
}

impl Slot {
    pub const NAN: Self = Self {
        min: f32::NAN,
        p2_5: f32::NAN,
        p25: f32::NAN,
        median: f32::NAN,
        p75: f32::NAN,
        p97_5: f32::NAN,
        loss_pct: f32::NAN,
    };

    pub fn is_nan(&self) -> bool {
        self.min.is_nan()
            && self.p2_5.is_nan()
            && self.p25.is_nan()
            && self.median.is_nan()
            && self.p75.is_nan()
            && self.p97_5.is_nan()
            && self.loss_pct.is_nan()
    }

    pub fn fields(&self) -> [f32; NUM_FIELDS] {
        [
            self.min,
            self.p2_5,
            self.p25,
            self.median,
            self.p75,
            self.p97_5,
            self.loss_pct,
        ]
    }

    pub fn from_fields(f: [f32; NUM_FIELDS]) -> Self {
        Self {
            min: f[0],
            p2_5: f[1],
            p25: f[2],
            median: f[3],
            p75: f[4],
            p97_5: f[5],
            loss_pct: f[6],
        }
    }
}

/// A timestamped slot for range queries.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub timestamp_secs: i64,
    pub slot: Slot,
}
