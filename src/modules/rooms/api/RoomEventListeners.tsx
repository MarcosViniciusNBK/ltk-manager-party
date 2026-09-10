import { useQueryClient } from "@tanstack/react-query";

import { useTauriEvent } from "@/lib/useTauriEvent";

import { roomKeys } from "./keys";

/**
 * Refresh the local room view after backend progress. Event payloads remain owned by the room
 * boundary; invalidation avoids treating an event as a manifest or a game-affecting command.
 */
export function RoomEventListeners() {
  const queryClient = useQueryClient();

  const refreshRooms = () => {
    void queryClient.invalidateQueries({ queryKey: roomKeys.all });
  };

  useTauriEvent("room-sync-progress", refreshRooms);
  useTauriEvent("room-transfer-progress", refreshRooms);
  useTauriEvent("room-presence-changed", refreshRooms);

  return null;
}
