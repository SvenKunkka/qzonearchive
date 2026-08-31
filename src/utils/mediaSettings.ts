// 媒体下载模式设置（仅保存在本机，不包含任何账号信息）
const MEDIA_DOWNLOAD_MODE_KEY = "qzone-media-download-mode";

export type MediaDownloadMode = "data-only" | "images" | "full";

export const MEDIA_MODE_OPTIONS: { label: string; value: MediaDownloadMode; hint: string }[] = [
  { label: "仅保存数据", value: "data-only", hint: "只保存动态与互动记录，不下载任何媒体" },
  { label: "下载图片", value: "images", hint: "下载动态图片与视频封面，本地可离线查看" },
  { label: "完整下载", value: "full", hint: "下载图片和视频（占用空间较大）" },
];

export function getMediaDownloadMode(): MediaDownloadMode {
  const value = localStorage.getItem(MEDIA_DOWNLOAD_MODE_KEY);
  return value === "images" || value === "full" ? value : "data-only";
}

export function setMediaDownloadMode(value: MediaDownloadMode) {
  localStorage.setItem(MEDIA_DOWNLOAD_MODE_KEY, value);
}
