pub trait Compressor {
    fn compress(&mut self, input: &[u8]) -> Vec<u8>;
}
