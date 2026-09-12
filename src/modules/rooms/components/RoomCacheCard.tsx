import { BroomIcon } from "@phosphor-icons/react";

import { Button, SectionCard, useToast } from "@/components";
import { errorMessage, errorTitle, m } from "@/i18n";

import { usePruneRoomCache, useRoomCacheStatus } from "../api";

export function RoomCacheCard() {
  const toast = useToast();
  const { data: cache } = useRoomCacheStatus();
  const pruneCache = usePruneRoomCache();

  function prune() {
    pruneCache.mutate(undefined, {
      onSuccess: () =>
        toast.success(m.rooms_cache_pruned_title(), m.rooms_cache_pruned_description()),
      onError: (error) =>
        toast.error(errorTitle(error, m.rooms_cache_prune_failed_title()), errorMessage(error)),
    });
  }

  return (
    <SectionCard
      title={m.rooms_cache_title()}
      description={m.rooms_cache_description()}
      icon={<BroomIcon className="h-4 w-4" />}
      action={
        <Button
          size="sm"
          variant="ghost"
          left={<BroomIcon weight="bold" />}
          onClick={prune}
          loading={pruneCache.isPending}
        >
          {m.rooms_prune_cache_action()}
        </Button>
      }
    >
      <p className="text-sm text-surface-200">
        {m.rooms_cache_summary({
          blobs: cache?.referencedBlobs ?? 0,
          rooms: cache?.joinedRooms ?? 0,
        })}
      </p>
      <p className="text-xs text-surface-400">
        {m.rooms_cache_transfers({ count: cache?.pendingTransfers ?? 0 })}
      </p>
    </SectionCard>
  );
}
