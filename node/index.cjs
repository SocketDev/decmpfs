'use strict'

// The N-API binding exposes `writeDecmpfsFile` (async) + `writeDecmpfsFileSync`.
// This loads the host-built addon; a multi-platform loader (per-triple `.node`
// selection) is the napi CLI's packaging job and a publishing follow-up.
module.exports = require('./decmpfs.node')
