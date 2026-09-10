import { FolderSimpleIcon, KeyIcon, PlusCircleIcon } from "@phosphor-icons/react";
import { useState } from "react";

import { Button, FormField, SectionCard, useToast } from "@/components";
import { errorMessage, m } from "@/i18n";

import { useCreateRoomDraft, useOpenRoomDraft } from "../api";

export function RoomDraftForm({ onOpened }: { onOpened: (roomId: string) => void }) {
  const [roomCode, setRoomCode] = useState("");
  const toast = useToast();
  const createDraft = useCreateRoomDraft();
  const openDraft = useOpenRoomDraft();
  const busy = createDraft.isPending || openDraft.isPending;

  function requestedRoomCode(): string | null {
    const value = roomCode.trim();
    if (value) return value;
    toast.warning(m.rooms_code_required());
    return null;
  }

  function create() {
    const roomId = requestedRoomCode();
    if (!roomId) return;
    createDraft.mutate(roomId, {
      onSuccess: (room) => {
        onOpened(room.roomId);
        toast.success(m.rooms_created_title(), m.rooms_draft_ready_description());
      },
      onError: (error) => toast.error(m.rooms_open_failed_title(), errorMessage(error)),
    });
  }

  function open() {
    const roomId = requestedRoomCode();
    if (!roomId) return;
    openDraft.mutate(roomId, {
      onSuccess: (room) => {
        onOpened(room.roomId);
        toast.success(m.rooms_opened_title(), m.rooms_draft_ready_description());
      },
      onError: (error) => toast.error(m.rooms_open_failed_title(), errorMessage(error)),
    });
  }

  return (
    <SectionCard
      title={m.rooms_join_title()}
      description={m.rooms_join_description()}
      icon={<KeyIcon className="h-4 w-4" />}
    >
      <div className="grid gap-4 sm:grid-cols-2">
        <FormField
          label={m.rooms_code_label()}
          description={m.rooms_code_description()}
          placeholder={m.rooms_code_placeholder()}
          value={roomCode}
          onChange={(event) => setRoomCode(event.target.value)}
          autoComplete="off"
        />
        <FormField
          label={m.rooms_password_label()}
          description={m.rooms_password_description()}
          placeholder={m.rooms_password_placeholder()}
          type="password"
          disabled
        />
      </div>
      <div className="flex flex-wrap gap-2">
        <Button
          variant="filled"
          left={<PlusCircleIcon weight="bold" />}
          onClick={create}
          loading={createDraft.isPending}
          disabled={busy}
        >
          {m.rooms_create_draft_action()}
        </Button>
        <Button
          variant="outline"
          left={<FolderSimpleIcon weight="bold" />}
          onClick={open}
          loading={openDraft.isPending}
          disabled={busy}
        >
          {m.rooms_open_draft_action()}
        </Button>
      </div>
    </SectionCard>
  );
}
