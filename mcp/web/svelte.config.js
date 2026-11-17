import adapter from "@sveltejs/adapter-static";
export default {
  kit: {
    adapter: adapter({ pages: "../public", assets: "../public" }),
    paths: { relative: false },
    version: { name: "super-dashboard-v1" },
    csp: {
      mode: "hash",
      directives: {
        "script-src": ["self"],
        "object-src": ["none"],
        "base-uri": ["none"],
      },
    },
  },
};
