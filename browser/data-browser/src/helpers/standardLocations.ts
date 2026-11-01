import { ai, core, dataBrowser, type Store } from '@tomic/react';
import {
  aiChatsFolderCert,
  decodeB64,
  getOrCreateDriveLocation,
  isNotAvailableLocally,
  isNotFound,
} from '@tomic/lib';

/** Coalesce first use in a store; separate devices derive the same subject. */
const aiFolderRequests = new WeakMap<Store, Map<string, Promise<string>>>();

const DRIVE_READ_PATIENCE_MS = 15_000;

/**
 * A newly initialized home drive can briefly report not-found. Re-read from
 * the server so one cached local miss does not permanently block chat creation.
 */
async function readDriveForChats(store: Store, driveSubject: string) {
  const first = await store.getResource(driveSubject);

  if (!first.error) return first;

  let drive = first;
  let wait = 250;
  const until = Date.now() + DRIVE_READ_PATIENCE_MS;

  while (drive.error && Date.now() < until) {
    const { promise, resolve } = Promise.withResolvers<void>();
    setTimeout(resolve, wait);
    await promise;
    wait = Math.min(wait * 2, 2_000);

    try {
      drive = await store.fetchResourceFromServer(driveSubject);
    } catch {
      // A failed retry leaves the original resource error for the caller.
    }
  }

  return drive;
}

export async function getOrCreateAiChatsFolder(
  store: Store,
  driveSubject: string,
): Promise<string> {
  const agent = store.getAgent();
  if (!agent) throw new Error('Sign in to create an AI chat.');
  let requests = aiFolderRequests.get(store);

  if (!requests) {
    requests = new Map();
    aiFolderRequests.set(store, requests);
  }

  const key = `${agent.subject}\0${driveSubject}`;
  const pending = requests.get(key);
  if (pending) return pending;
  const request = ensureAiChatsFolder(store, driveSubject);
  requests.set(key, request);

  try {
    return await request;
  } finally {
    requests.delete(key);
  }
}

async function ensureAiChatsFolder(
  store: Store,
  driveSubject: string,
): Promise<string> {
  const agent = store.getAgent()!;
  const drive = await readDriveForChats(store, driveSubject);
  if (drive.error) throw drive.error;
  let subject: string;

  try {
    subject = await agent.aiChatsFolderSubject(driveSubject);
  } catch (error) {
    // A valid existing pointer is the only safe fallback for an old
    // non-extractable session.
    const pointer = drive.get(ai.properties.aiChatsFolder) as
      | string
      | undefined;

    if (pointer) {
      const existing = await store.getResource(pointer);
      if (!existing.error && existing.hasClasses(dataBrowser.classes.folder))
        return pointer;
    }

    throw error;
  }

  if (store.isDestroyed(subject)) {
    throw new Error(
      'The AI Chats folder was deleted. Restore it before creating a chat.',
    );
  }

  const existing = await store.getResource(subject);

  if (existing.error) {
    if (!isNotFound(existing.error) && !isNotAvailableLocally(existing.error))
      throw existing.error;
    const folder = await store.newResource({
      subject,
      genesisCert: aiChatsFolderCert(
        decodeB64(await agent.getPublicKey()),
        driveSubject,
      ),
      parent: driveSubject,
      isA: dataBrowser.classes.folder,
      propVals: { [core.properties.name]: /* @wc-ignore */ 'AI Chats' },
    });
    await folder.save();
    store.notifyResourceManuallyCreated(folder);
  } else if (!existing.hasClasses(dataBrowser.classes.folder)) {
    throw new Error('The AI Chats location is not a folder.');
  }

  if (drive.get(ai.properties.aiChatsFolder) !== subject) {
    await drive.set(ai.properties.aiChatsFolder, subject);
    await drive.save();
  }

  return subject;
}

/** The Drive's Comments folder: default parent (= rights anchor) for comment Messages. */
export async function getOrCreateCommentsFolder(
  store: Store,
  driveSubject: string,
): Promise<string> {
  return getOrCreateDriveLocation(
    store,
    driveSubject,
    dataBrowser.properties.commentsFolder,
    { isA: dataBrowser.classes.folder, name: /* @wc-ignore */ 'Comments' },
  );
}

/** The Drive's Trash folder: where "Delete" in the sidebar parks resources. */
export const getOrCreateTrashFolder = (
  store: Store,
  driveSubject: string,
): Promise<string> => store.getOrCreateTrashFolder(driveSubject);

/** The Drive's follow-sessions ChatRoom: while an agent is being followed,
 *  their client logs which resources they visit here (issue #1229). */
export async function getOrCreateFollowSessionsChatroom(
  store: Store,
  driveSubject: string,
): Promise<string> {
  return getOrCreateDriveLocation(
    store,
    driveSubject,
    dataBrowser.properties.followSessionsChatroom,
    {
      isA: dataBrowser.classes.chatroom,
      name: /* @wc-ignore */ 'Follow sessions',
    },
  );
}
