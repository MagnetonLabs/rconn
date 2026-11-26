import data from '../releases.json';

export type ReleaseFile = (typeof data.files)[number];

export const version = data.version;
export const files = data.files;

export const url = (f: ReleaseFile) => new URL(f.file, data.baseUrl).href;

/** Decimal megabytes, the way file managers on macOS and Linux show them. */
export const size = (f: ReleaseFile) => `${Math.round(f.sizeBytes / 1_000_000)} MB`;

export const byId = (id: string) => {
  const f = files.find((x) => x.id === id);
  if (!f) throw new Error(`releases.json has no file with id "${id}"`);
  return f;
};

/** The compact form the download button's script reads. */
export const forScript = () =>
  JSON.stringify(
    Object.fromEntries(
      files.map((f) => [f.id, { href: url(f), name: f.name, arch: f.archLabel, size: size(f), version: data.version }]),
    ),
  );
