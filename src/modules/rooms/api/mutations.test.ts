import { QueryClient } from "@tanstack/react-query";

import { roomMutations } from "./mutations";

it("marks room mutations with local toast handling as silent globally", () => {
  const client = new QueryClient();

  expect(roomMutations.createRemote(client).meta).toEqual({ silentError: true });
  expect(roomMutations.joinRemote(client).meta).toEqual({ silentError: true });
  expect(roomMutations.publishProfile(client).meta).toEqual({ silentError: true });
  expect(roomMutations.syncProfile(client).meta).toEqual({ silentError: true });
  expect(roomMutations.leave(client).meta).toEqual({ silentError: true });
  expect(roomMutations.pruneCache(client).meta).toEqual({ silentError: true });
});
