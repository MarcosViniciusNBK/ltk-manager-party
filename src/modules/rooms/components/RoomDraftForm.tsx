import { FolderSimpleIcon, KeyIcon, PlusCircleIcon } from "@phosphor-icons/react";
import { useState } from "react";

import { Button, FormField, SectionCard, useToast } from "@/components";
import { errorMessage, m } from "@/i18n";
import type { JoinedRoom } from "@/lib/bindings.gen";

import { useCreateRemoteRoom, useJoinRemoteRoom } from "../api";

export function RoomDraftForm({ onOpened }: { onOpened: (roomId: string) => void }) {
  const [roomCode, setRoomCode] = useState("");
  const [password, setPassword] = useState("");
  const toast = useToast();
  const createRemote = useCreateRemoteRoom();
  const joinRemote = useJoinRemoteRoom();
  const busy = createRemote.isPending || joinRemote.isPending;

  function requestedCredentials(): { roomId: string; password: string } | null {
    const roomId = roomCode.trim();
    if (!roomId) {
      toast.warning(m.rooms_code_required());
      return null;
    }
    const pwd = password.trim();
    if (!pwd) {
      toast.warning(m.rooms_password_required());
      return null;
    }
    return { roomId, password: pwd };
  }

  function create() {
    const creds = requestedCredentials();
    if (!creds) return;
    createRemote.mutate(creds, {
      onSuccess: (room: JoinedRoom) => {
        onOpened(room.roomId);
        toast.success(m.rooms_created_title(), m.rooms_draft_ready_description());
      },
      onError: (error: unknown) => toast.error(m.rooms_open_failed_title(), errorMessage(error)),
    });
  }

  function join() {
    const creds = requestedCredentials();
    if (!creds) return;
    joinRemote.mutate(creds, {
      onSuccess: (room: JoinedRoom) => {
        onOpened(room.roomId);
        toast.success(m.rooms_opened_title(), m.rooms_draft_ready_description());
      },
      onError: (error: unknown) => toast.error(m.rooms_open_failed_title(), errorMessage(error)),
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
          value={password}
          onChange={(event) => setPassword(event.target.value)}
          autoComplete="current-password"
        />
      </div>
      <div className="flex flex-wrap gap-2">
        <Button
          variant="filled"
          left={<PlusCircleIcon weight="bold" />}
          onClick={create}
          loading={createRemote.isPending}
          disabled={busy}
        >
          {m.rooms_create_draft_action()}
        </Button>
        <Button
          variant="outline"
          left={<FolderSimpleIcon weight="bold" />}
          onClick={join}
          loading={joinRemote.isPending}
          disabled={busy}
        >
          {m.rooms_open_draft_action()}
        </Button>
      </div>
    </SectionCard>
  );
}
