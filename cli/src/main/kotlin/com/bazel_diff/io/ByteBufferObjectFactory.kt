package com.bazel_diff.io

import com.bazel_diff.extensions.compatClear
import org.apache.commons.pool2.PooledObject
import org.apache.commons.pool2.PooledObjectFactory
import org.apache.commons.pool2.impl.DefaultPooledObject
import java.nio.ByteBuffer

class ByteBufferObjectFactory(private val bufferSize: Int) : PooledObjectFactory<ByteBuffer> {
    override fun activateObject(p: PooledObject<ByteBuffer>) {
        p.getObject().compatClear()
    }

    override fun destroyObject(p: PooledObject<ByteBuffer>) {
        p.getObject().compatClear()
    }

    override fun makeObject(): PooledObject<ByteBuffer?> {
        return DefaultPooledObject<ByteBuffer>(ByteBuffer.allocate(bufferSize))
    }

    override fun passivateObject(p: PooledObject<ByteBuffer>) {
        p.getObject().compatClear()
    }

    override fun validateObject(p: PooledObject<ByteBuffer>): Boolean {
        return p.getObject().capacity() == bufferSize && !p.getObject().isDirect
    }
}
