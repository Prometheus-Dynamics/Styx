//! Convergence measurements over a series of per-frame values.

/// How a series settled after a change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Convergence {
    /// The settled value: mean of the last `tail` values.
    pub final_value: f64,
    /// Frames from the change until the series stays within the tolerance of the final value
    /// (`None` if it never does before the tail).
    pub settle_frames: Option<usize>,
    /// Largest relative excursion past the final value, in the direction of approach.
    pub overshoot: f64,
    /// Relative standard deviation over the tail (steady-state jitter).
    pub jitter: f64,
}

/// Measure `values[from..]` settling to within `tolerance` (relative), using the last `tail`
/// values as the settled value.
pub fn convergence(values: &[f64], from: usize, tolerance: f64, tail: usize) -> Convergence {
    let tail = tail.clamp(1, values.len().max(1));
    let last = &values[values.len() - tail..];
    let final_value = last.iter().sum::<f64>() / tail as f64;
    let var = last.iter().map(|v| (v - final_value).powi(2)).sum::<f64>() / tail as f64;
    let jitter = var.sqrt() / final_value.abs().max(1e-12);
    let series = &values[from.min(values.len())..];
    let within = |v: &f64| (v - final_value).abs() <= tolerance * final_value.abs();
    let settle_frames = (0..series.len())
        .find(|&i| series[i..].iter().all(within))
        .filter(|&i| i + tail <= series.len() || series.len() <= tail);
    let start = series.first().copied().unwrap_or(final_value);
    let overshoot = if start <= final_value {
        series
            .iter()
            .fold(0.0f64, |m, v| m.max(v / final_value - 1.0))
    } else {
        series
            .iter()
            .fold(0.0f64, |m, v| m.max(1.0 - v / final_value))
    };
    Convergence {
        final_value,
        settle_frames,
        overshoot,
        jitter,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_settling_and_overshoot() {
        let mut v = vec![0.1; 5];
        v.extend([0.5, 0.9, 1.1, 1.02, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        let c = convergence(&v, 5, 0.05, 4);
        assert_eq!(c.final_value, 1.0);
        assert_eq!(c.settle_frames, Some(3));
        assert!((c.overshoot - 0.1).abs() < 1e-9);
        assert_eq!(c.jitter, 0.0);
    }
}
