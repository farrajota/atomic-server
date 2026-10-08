const RASTER_IMAGE_TYPES = new Set([
  'image/apng',
  'image/avif',
  'image/bmp',
  'image/gif',
  'image/heic',
  'image/heif',
  'image/jpeg',
  'image/png',
  'image/tiff',
  'image/vnd.microsoft.icon',
  'image/webp',
  'image/x-icon',
]);

export const SVG_MEDIA_TYPE = 'image/svg+xml';
const OPAQUE_TYPE = 'application/octet-stream';

/**
 * The type bytes are shown under when turned into a URL for an `<img>`,
 * `<video>` or similar. The type normally comes from a File's `mimetype`,
 * which whoever uploaded it chose. A `blob:` URL is same-origin with the app,
 * so one typed as a document (HTML, XML, SVG) would run its scripts with the
 * app's privileges when opened. Media types keep their type; SVG is reported
 * as such so callers can show it as a `data:` URL (see {@link svgDataUrl});
 * everything else becomes opaque bytes.
 */
export function displayMediaType(contentType: string | undefined): string {
  const type = (contentType ?? '').split(';')[0].trim().toLowerCase();

  if (
    RASTER_IMAGE_TYPES.has(type) ||
    type.startsWith('video/') ||
    type.startsWith('audio/') ||
    type === 'application/pdf'
  ) {
    return type;
  }

  return type === SVG_MEDIA_TYPE ? SVG_MEDIA_TYPE : OPAQUE_TYPE;
}

/**
 * An SVG as a `data:` URL. An `<img>` renders it without running its
 * scripts, and browsers refuse to navigate to one, so unlike a `blob:` URL it
 * never becomes a same-origin document. It holds no memory to revoke.
 */
export function svgDataUrl(bytes: Uint8Array): string {
  let binary = '';

  for (const byte of bytes) binary += String.fromCharCode(byte);

  return `data:${SVG_MEDIA_TYPE};base64,${btoa(binary)}`;
}
