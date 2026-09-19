import { rmSync } from "node:fs";

export default function globalTeardown() {
  // `global-setup` makes a directory per run, and /tmp is a tmpfs on this
  // project's usual machines — left alone it would cost real memory per run.
  //
  // Kept when `E2E_DEBUG` asked for artifacts worth reading afterwards, and
  // when the root was handed in rather than made here, because then it is
  // someone else's to delete.
  if (process.env.E2E_DEBUG || !process.env.LEDECKY_TEST_ROOT_OWNED) return;

  rmSync(process.env.LEDECKY_TEST_ROOT, { recursive: true, force: true });
}
