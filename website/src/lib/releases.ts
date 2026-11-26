import data from '../releases.json';

export type ReleaseFile = (typeof data.files)[number];

/** Who a file installs for: "system" needs an administrator and serves every account
 *  (and the sign-in screen); "user" installs for the signed-in account only; "any" is
 *  one file that does either (the Mac disk image). */
export type Scope = 'system' | 'user' | 'any';

export const version = data.version;
export const files = data.files;

export const url = (f: ReleaseFile) => new URL(f.file, data.baseUrl).href;

/** Decimal megabytes, the way file managers on macOS and Linux show them. Empty until the
 *  release script has filled in the size. */
export const size = (f: ReleaseFile) => (f.sizeBytes > 0 ? `${Math.round(f.sizeBytes / 1_000_000)} MB` : '');

export const byId = (id: string) => {
  const f = files.find((x) => x.id === id);
  if (!f) throw new Error(`releases.json has no file with id "${id}"`);
  return f;
};

/** The compact form the download button's script reads. */
export const forScript = () =>
  JSON.stringify(
    Object.fromEntries(
      files.map((f) => [
        f.id,
        { href: url(f), name: f.name, arch: f.archLabel, size: size(f), version: data.version, scope: f.scope },
      ]),
    ),
  );
