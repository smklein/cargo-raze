pub type RazeResult<T> = Result<T, RazeErr>;

#[derive(Debug, Clone)]
pub struct RazeErr {
  pub component: String,
  pub message: String,
}
