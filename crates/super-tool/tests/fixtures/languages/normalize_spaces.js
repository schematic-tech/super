import assert from "node:assert/strict";
import { assume, supertest } from "schematic-supertest";
import { collapseSpaces } from "../src/text.js";

export const normalizingTwiceChangesNothing = supertest((text) => {
  assume(typeof text === "string");
  const once = collapseSpaces(text);
  const twice = collapseSpaces(once);

  assert.equal(twice, once);
});
