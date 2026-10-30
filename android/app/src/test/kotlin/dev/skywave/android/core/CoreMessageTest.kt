package dev.skywave.android.core

import dev.skywave.core.SkywaveException
import org.junit.Assert.assertEquals
import org.junit.Test

class CoreMessageTest {
    @Test
    fun coreErrorsKeepTheirKindAndReason() {
        assertEquals(CoreMessage.Storage("full"), describe(SkywaveException.Store("full")))
        assertEquals(CoreMessage.Io("gone"), describe(SkywaveException.Io("gone")))
        assertEquals(CoreMessage.InvalidPairUri("bad"), describe(SkywaveException.InvalidPairUri("bad")))
        assertEquals(CoreMessage.ShutDown, describe(SkywaveException.ShutDown()))
    }

    @Test
    fun anythingElseIsShownAsIs() {
        assertEquals(CoreMessage.Other("boom"), describe(IllegalStateException("boom")))
        assertEquals(CoreMessage.Other("IllegalStateException"), describe(IllegalStateException()))
    }
}
