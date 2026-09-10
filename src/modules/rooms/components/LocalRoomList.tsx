import { ArrowRightIcon, UsersThreeIcon } from "@phosphor-icons/react";

import { Code, EmptyState, SectionCard } from "@/components";
import { m } from "@/i18n";

import { useRoomMemberships } from "../api";

export function LocalRoomList({
  selectedRoomId,
  onSelect,
}: {
  selectedRoomId: string;
  onSelect: (roomId: string) => void;
}) {
  const { data: rooms = [] } = useRoomMemberships();

  return (
    <SectionCard
      title={m.rooms_memberships_title()}
      description={m.rooms_memberships_description()}
      icon={<UsersThreeIcon className="h-4 w-4" />}
    >
      {rooms.length === 0 && (
        <EmptyState
          size="sm"
          icon={<UsersThreeIcon className="h-12 w-12" />}
          title={m.rooms_empty_title()}
          description={m.rooms_empty_description()}
        />
      )}
      {rooms.length > 0 && (
        <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-3">
          {rooms.map((room) => {
            const selected = room.roomId === selectedRoomId;
            return (
              <button
                key={room.roomId}
                type="button"
                aria-pressed={selected}
                onClick={() => onSelect(room.roomId)}
                className="flex min-w-0 items-center justify-between gap-3 rounded-lg border border-surface-700 bg-surface-800/50 p-3 text-left transition-colors hover:border-accent-hover hover:bg-surface-800 aria-pressed:border-accent-500 aria-pressed:bg-accent-500/10"
              >
                <span className="min-w-0">
                  <Code className="block truncate">{room.roomId}</Code>
                  <span className="mt-1 block truncate text-xs text-surface-400">
                    {m.rooms_local_member_label()}: {room.memberId}
                  </span>
                </span>
                <ArrowRightIcon className="h-4 w-4 shrink-0 text-surface-400" />
              </button>
            );
          })}
        </div>
      )}
    </SectionCard>
  );
}
