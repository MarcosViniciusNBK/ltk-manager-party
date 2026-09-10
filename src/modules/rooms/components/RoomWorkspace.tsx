import { CheckCircleIcon } from "@phosphor-icons/react";
import { useEffect, useState } from "react";

import { AlertBox } from "@/components";
import { m } from "@/i18n";

import { RoomEventListeners, useRoomMemberships } from "../api";
import { LocalRoomList } from "./LocalRoomList";
import { RoomCacheCard } from "./RoomCacheCard";
import { RoomDetail } from "./RoomDetail";
import { RoomDraftForm } from "./RoomDraftForm";

/** The room page only coordinates isolated room state and explicit library preparation. */
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

      <AlertBox
        variant="success"
        title={m.rooms_service_pending_title()}
        icon={<CheckCircleIcon className="h-5 w-5 text-emerald-400" />}
      >
        {m.rooms_service_pending_description()}
      </AlertBox>

      <RoomDraftForm onOpened={setSelectedRoomId} />
      <LocalRoomList selectedRoomId={selectedRoomId} onSelect={setSelectedRoomId} />
      {selectedRoomId && <RoomDetail roomId={selectedRoomId} />}
      <RoomCacheCard />
    </div>
  );
}
