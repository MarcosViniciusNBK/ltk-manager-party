import { createFileRoute } from "@tanstack/react-router";

import { RoomWorkspace } from "@/modules/rooms";

export const Route = createFileRoute("/rooms")({
  component: RoomWorkspace,
});
