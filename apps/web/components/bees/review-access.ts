export function motionReviewEnabled() {
  return process.env.NODE_ENV === "development" || process.env.BEE_ENABLE_MOTION_REVIEW === "1";
}
