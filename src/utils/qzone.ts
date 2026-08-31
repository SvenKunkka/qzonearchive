import { invoke } from "@tauri-apps/api/core";

export interface FeedPage {
  feeds: Record<string, unknown>[];
  attachInfo?: string;
  hasMore: boolean;
}

export function fetchFirstFeeds() {
  return invoke<FeedPage>("fetch_first_feeds");
}

export function fetchMoreFeeds(attachInfo: string) {
  return invoke<FeedPage>("fetch_more_feeds", { attachInfo });
}

export type ArchiveStatus = "idle" | "running" | "completed" | "cancelled" | "limited" | "error";
export interface BatchRetryProgress { current: number; total: number; recovered: number; failed: number; recoveredRecords: number; }
export interface ArchiveProgress { status: ArchiveStatus; pages: number; fetched: number; saved: number; skipped: number; message: string; retryAt?: number; batchRetry?: BatchRetryProgress; }
export interface ArchiveSkipItem {
  id: number; pageNumber: number; cursorOffset: number; offsetAdvance: number; baseTime: number;
  error: string; skippedAt: number; retryCount: number; lastRetryAt?: number; resolvedAt?: number; recoveredRecords: number;
}
export interface ArchiveSkipRetryResult { success: boolean; message: string; recoveredRecords: number; }
export const clearResolvedArchiveSkips = () => invoke<number>("clear_resolved_archive_skips");
export interface ArchiveSkipBatchRetryResult { total: number; recovered: number; failed: number; recoveredRecords: number; }
export interface ArchiveItem {
  id: number; cellId: string; publishedAt: number; content?: string; authorUin?: string;
  authorName?: string; pictureUrls: string[]; videoUrl?: string; videoUrls: string[]; videoCoverUrl?: string; likeCount: number; commentCount: number;
  likes: LikeUser[];
  comments: ArchiveComment[];
}
export interface LikeUser { uin?: string; nickname?: string; }
export interface ArchiveReply { uin?: string; nickname?: string; replyToUin?: string; replyToNickname?: string; content: string; createdAt: number; }
export interface ArchiveComment { uin?: string; nickname?: string; content: string; createdAt: number; replies: ArchiveReply[]; }
export type ArchiveCategory = "self" | "other" | "guestbook";
export interface ArchiveMediaItem { key: string; dynamicId: number; mediaType: "photo" | "video"; pictureIndex?: number; url: string; coverUrl?: string; publishedAt: number; authorUin?: string; authorName?: string; content?: string; }
export interface ArchiveMediaPage { items: ArchiveMediaItem[]; total: number; years: number[]; }
export const startFeedArchive = (intervalMs: number, incremental = true) => invoke<ArchiveProgress>("start_feed_archive", { intervalMs, incremental });
export const getArchiveProgress = () => invoke<ArchiveProgress>("get_archive_progress");
export const cancelFeedArchive = () => invoke<void>("cancel_feed_archive");
export const listArchiveSkips = () => invoke<ArchiveSkipItem[]>("list_archive_skips");
export const retryArchiveSkip = (id: number) => invoke<ArchiveSkipRetryResult>("retry_archive_skip", { id });
export const retryAllArchiveSkips = (intervalMs: number) => invoke<ArchiveSkipBatchRetryResult>("retry_all_archive_skips", { intervalMs });
export const listArchivedFeeds = (limit = 100, offset = 0, category: ArchiveCategory = "self") => invoke<ArchiveItem[]>("list_archived_feeds", { limit, offset, category });
export const listArchivedMedia = (limit = 60, offset = 0, year?: number) => invoke<ArchiveMediaPage>("list_archived_media", { limit, offset, year });
export const getArchivedFeed = (id: number) => invoke<ArchiveItem>("get_archived_feed", { id });
export const countArchivedFeeds = (category: ArchiveCategory = "self") => invoke<number>("count_archived_feeds", { category });
export const exportArchivedHtml = (category: ArchiveCategory, ids?: number[]) => invoke<string>("export_archived_html", { category, ids });
export const loadArchivedImage = (id: number, pictureIndex: number) => invoke<string>("load_archived_image", { id, pictureIndex });
export const loadArchivedVideo = (id: number) => invoke<string>("load_archived_video", { id });
export interface ArchiveOverview { dynamics: number; pictures: number; comments: number; likes: number; databaseBytes: number; }
export const getArchiveOverview = () => invoke<ArchiveOverview>("get_archive_overview");
export interface Interactor { uin: string; nickname: string; likes: number; comments: number; total: number; lastAt: number; }
export const listInteractors = () => invoke<Interactor[]>("list_interactors");
export interface InteractionRank { uin: string; nickname: string; interactions: number; likes: number; comments: number; }
export const getInteractionRanking = (limit = 8) => invoke<InteractionRank[]>("get_interaction_ranking", { limit });
export const deleteArchivedFeeds = (ids: number[]) => invoke<number>("delete_archived_feeds", { ids });
export const clearArchivedFeeds = () => invoke<number>("clear_archived_feeds");
export const deleteAllAppData = () => invoke<void>("delete_all_app_data");

export const openRecyclePasswordWindow = () => invoke<void>("open_recycle_password_window");
export const checkRecyclePassword = () => invoke<string | null>("check_recycle_password");
export const closeRecyclePasswordWindow = () => invoke<void>("close_recycle_password_window");
export const listRecycleAlbums = (pwd2sig: string) => invoke<Record<string, unknown>>("list_recycle_albums", { pwd2sig });
export const listRecyclePhotos = (pwd2sig: string, albumId?: string) => invoke<Record<string, unknown>>("list_recycle_photos", { pwd2sig, albumId });
export const listQzoneAlbums = () => invoke<Record<string, unknown>>("list_qzone_albums");
export const createQzoneAlbum = (name: string) => invoke<Record<string, unknown>>("create_qzone_album", { name });
export const recoverRecycleAlbum = (pwd2sig: string, albumId: string) => invoke<Record<string, unknown>>("recover_recycle_album", { pwd2sig, albumId });
export const recoverRecyclePhotos = (pwd2sig: string, sourceAlbumId: string, targetAlbumId: string, photoIds: string[]) =>
  invoke<Record<string, unknown>>("recover_recycle_photos", { pwd2sig, sourceAlbumId, targetAlbumId, photoIds });
export const loadRecyclePhotoPreview = (imageUrl: string) => invoke<string>("load_recycle_photo_preview", { imageUrl });

// ---- 本地媒体归档 ----
export type MediaDownloadMode = "data-only" | "images" | "full";
export interface MediaDownloadProgress {
  status: "idle" | "running" | "paused" | "cancelled" | "completed" | "error";
  total: number;
  done: number;
  failed: number;
  skipped: number;
  bytesDone: number;
  currentUrl?: string;
  message: string;
}
export interface MediaItemInfo {
  id: number;
  dynamicId?: number;
  mediaKind: string;
  remoteUrl: string;
  localPath?: string;
  sha256?: string;
  sizeBytes?: number;
  mimeType?: string;
  downloadStatus: string;
  downloadAttempts: number;
  lastError?: string;
  lastDownloadedAt?: number;
  createdAt: number;
}
export interface MediaStats {
  total: number;
  pending: number;
  done: number;
  failed: number;
  paused: number;
  skipped: number;
  bytesDone: number;
  images: number;
  videos: number;
}
export interface MediaSyncResult { created: number; total: number; }
export const syncMediaItems = () => invoke<MediaSyncResult>("sync_media_items");
export const startMediaDownload = (mode: MediaDownloadMode, retryFailed = false) =>
  invoke<MediaDownloadProgress>("start_media_download", { mode, retryFailed });
export const getMediaDownloadProgress = () => invoke<MediaDownloadProgress>("get_media_download_progress");
export const pauseMediaDownload = () => invoke<void>("pause_media_download");
export const resumeMediaDownload = () => invoke<void>("resume_media_download");
export const cancelMediaDownload = () => invoke<void>("cancel_media_download");
export const listMediaItems = (limit = 50, offset = 0, statusFilter?: string) =>
  invoke<MediaItemInfo[]>("list_media_items", { limit, offset, statusFilter });
export const getMediaStats = () => invoke<MediaStats>("get_media_stats");

// ---- 数据源同步状态 ----
export interface SourceStateInfo {
  source: string;
  cursor: string;
  status: string;
  lastSyncAt?: number;
  nextSyncAt?: number;
  lastError?: string;
  totalFetched: number;
  totalSaved: number;
  updatedAt: number;
}
export interface AlbumSyncResult { listed: number; saved: number; remoteMarked: number; total: number; }
export const listSourceStates = () => invoke<SourceStateInfo[]>("list_source_states_command");
export const resetSourceState = (source: string) => invoke<void>("reset_source_state_command", { source });
export const syncAlbumList = () => invoke<AlbumSyncResult>("sync_album_list_command");
