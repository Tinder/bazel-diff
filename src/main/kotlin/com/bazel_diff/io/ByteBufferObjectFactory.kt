package com.bazel_diff.io

import org.apache.commons.pool2.PooledObject
import org.apache.commons.pool2.PooledObjectFactory
import org.apache.commons.pool2.impl.DefaultPooledObject
import java.nio.ByteBuffer

class ByteBufferObjectFactory(private val bufferSize: Int) : PooledObjectFactory<ByteBuffer> {
    override fun activateObject(p: PooledObject<ByteBuffer>) {
        p.getObject()!!.clear()
    }

    override fun destroyObject(p: PooledObject<ByteBuffer>) {
        p.getObject()!!.clear()
    }

    override fun makeObject(): PooledObject<ByteBuffer?> {
        return DefaultPooledObject<ByteBuffer>(ByteBuffer.allocate(bufferSize))
    }

    override fun passivateObject(p: PooledObject<ByteBuffer>) {
        p.getObject()!!.clear()
    }

    override fun validateObject(p: PooledObject<ByteBuffer>): Boolean {
        return p.getObject()!!.capacity() == bufferSize && !p.getObject()!!.isDirect
    }
}