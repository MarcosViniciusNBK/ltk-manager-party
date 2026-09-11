import { useEffect, useState } from "react";

import { m } from "@/i18n";

import { RoomEventListeners, useRoomMemberships } from "../api";
import { LocalRoomList } from "./LocalRoomList";
import { RoomDetail } from "./RoomDetail";
import { RoomDraftForm } from "./RoomDraftForm";

/** Rooms synchronize their dedicated profile automatically; applying it remains user-driven. */
export function RoomWorkspace() {
  const [selectedRoomId, setSelectedRoomId] = useState("");
  const { data: rooms = [] } = useRoomMemberships();

  useEffect(() => {
    const selectedStillExists = rooms.some((room) => room.roomId === selectedRoomId);
    if (selectedStillExists) return;
    setSelectedRoomId(rooms[0]?.roomId ?? "");
  }, [rooms, selectedRoomId]);

  return (
    <div
      className="mx-auto flex h-full max-w-6xl flex-col gap-4 overflow-y-auto px-4 py-4"
      data-ui="RoomWorkspace"
    >
      <RoomEventListeners />
      <div>
        <h1 className="text-xl font-semibold text-surface-50">{m.rooms_page_title()}</h1>
        <p className="mt-1 max-w-3xl text-sm text-surface-400">{m.rooms_page_description()}</p>
      </div>

      <RoomDraftForm onOpened={setSelectedRoomId} />
      <LocalRoomList selectedRoomId={selectedRoomId} onSelect={setSelectedRoomId} />
      {selectedRoomId && <RoomDetail roomId={selectedRoomId} />}
    </div>
  );
}
