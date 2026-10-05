/// Maps input audio channels to output positions.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelMap {
    num_channels: usize,
    mapping: Vec<usize>,
}

impl ChannelMap {
    pub fn identity(n: usize) -> Self {
        ChannelMap {
            num_channels: n,
            mapping: (0..n).collect(),
        }
    }

    pub fn from_mapping(mapping: Vec<usize>) -> Self {
        let n = mapping.len();
        ChannelMap {
            num_channels: n,
            mapping,
        }
    }

    pub fn num_channels(&self) -> usize {
        self.num_channels
    }

    pub fn get(&self, input: usize) -> usize {
        self.mapping[input]
    }

    pub fn mapping(&self) -> &[usize] {
        &self.mapping
    }

    pub fn swap(&mut self, input_row: usize, target_output: usize) {
        if input_row >= self.num_channels || target_output >= self.num_channels {
            return;
        }
        let swapped_input = self
            .mapping
            .iter()
            .position(|&out| out == target_output)
            .unwrap_or(input_row);
        self.mapping.swap(input_row, swapped_input);
    }

    pub fn input_for_output(&self, output: usize) -> Option<usize> {
        self.mapping.iter().position(|&o| o == output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_map_identity_size() {
        let m = ChannelMap::identity(4);
        assert_eq!(m.num_channels(), 4);
        assert_eq!(m.mapping(), &[0, 1, 2, 3]);
    }

    #[test]
    fn test_channel_map_from_mapping() {
        let m = ChannelMap::from_mapping(vec![1, 0, 3, 2]);
        assert_eq!(m.num_channels(), 4);
        assert_eq!(m.get(0), 1);
        assert_eq!(m.get(2), 3);
    }

    #[test]
    fn test_channel_map_input_for_output() {
        let m = ChannelMap::from_mapping(vec![2, 0, 1]);
        assert_eq!(m.input_for_output(0), Some(1));
        assert_eq!(m.input_for_output(1), Some(2));
        assert_eq!(m.input_for_output(2), Some(0));
        assert_eq!(m.input_for_output(99), None);
    }

    #[test]
    fn test_channel_map_input_for_output_identity() {
        let m = ChannelMap::identity(3);
        assert_eq!(m.input_for_output(0), Some(0));
        assert_eq!(m.input_for_output(1), Some(1));
        assert_eq!(m.input_for_output(2), Some(2));
    }

    #[test]
    fn test_channel_map_swap() {
        let mut m = ChannelMap::from_mapping(vec![2, 0, 1]);
        m.swap(0, 0);
        assert_eq!(m.mapping(), &[0, 2, 1]);
        m.swap(0, 1);
        assert_eq!(m.mapping(), &[1, 2, 0]);
        // swap with itself — should be a no-op
        let initial = m.mapping().to_vec();
        m.swap(0, m.get(0));
        assert_eq!(m.mapping(), &initial);
    }
}
