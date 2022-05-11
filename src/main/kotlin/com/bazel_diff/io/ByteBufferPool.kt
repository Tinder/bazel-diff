package com.bazel_diff.io

import com.google.common.base.Suppliers
import org.apache.commons.pool2.impl.GenericObjectPool
import org.apache.commons.pool2.impl.GenericObjectPoolConfig
import java.nio.ByteBuffer

class ByteBufferPool(private val poolSize: Int, private val bufferSize: Int) {
    private val delegate = Suppliers.memoize {
        val config = GenericObjectPoolConfig<ByteBuffer?>()
        config.maxTotal = poolSize
        GenericObjectPool(ByteBufferObjectFactory(bufferSize), config)
    }

    fun borrow(): ByteBuffer? {
        return delegate.get().borrowObject()
    }

    fun recycle(buffer: ByteBuffer?) {
        delegate.get().returnObject(buffer)
    }
}

