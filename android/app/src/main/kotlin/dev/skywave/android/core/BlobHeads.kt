package dev.skywave.android.core

import dev.skywave.android.ui.screens.BlobHeadReader
import dev.skywave.core.readHead

/** The core reads blob heads (D2): bounded, invalid UTF-8 replaced, never the whole file. */
val coreBlobHeadReader = BlobHeadReader { path, max -> readHead(path, max) }
