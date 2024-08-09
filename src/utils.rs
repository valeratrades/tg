use rand::distributions::{Distribution, WeightedIndex};
use rand::{rngs::StdRng, SeedableRng};

fn zeta(s: f64, n: usize) -> f64 {
	(1..=n).map(|k| 1.0 / (k as f64).powf(s)).sum()
}

#[allow(unused)]
pub struct ZetaDistribution {
	alpha: f64,
	weights: Vec<f64>,
	normalization_constant: f64,
}

impl ZetaDistribution {
	pub fn new(alpha: f64, max_k: usize) -> ZetaDistribution {
		let normalization_constant = zeta(alpha, max_k);

		let mut weights = Vec::with_capacity(max_k);
		for k in 1..=max_k {
			weights.push((k as f64).powf(-alpha) / normalization_constant);
		}

		ZetaDistribution {
			alpha,
			weights,
			normalization_constant,
		}
	}

	pub fn sample(&self, seed: Option<u64>) -> usize {
		let mut rng = match seed {
			Some(s) => StdRng::seed_from_u64(s),
			None => StdRng::from_entropy(),
		};

		let dist = WeightedIndex::new(&self.weights).unwrap();
		dist.sample(&mut rng) + 1
	}
}
