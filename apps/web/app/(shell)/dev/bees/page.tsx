import { notFound } from "next/navigation";
import { motionReviewEnabled } from "@/components/bees/review-access";
import { MotionReview } from "./review";

export default function MotionReviewPage() {
  if (!motionReviewEnabled()) notFound();

  return <MotionReview />;
}
