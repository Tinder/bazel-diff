package com.bazel_diff;

import com.google.common.base.Supplier;
import com.google.common.base.Suppliers;
import org.apache.commons.pool2.PooledObject;
import org.apache.commons.pool2.PooledObjectFactory;
import org.apache.commons.pool2.impl.DefaultPooledObject;
import org.apache.commons.pool2.impl.GenericObjectPool;
import org.apache.commons.pool2.impl.GenericObjectPoolConfig;

import java.nio.ByteBuffer;

public class ByteBufferPool {
    private Integer poolSize;
    private Integer bufferSize;
    private Supplier<GenericObjectPool<ByteBuffer>> delegate = Suppliers.memoize(() -> {
        GenericObjectPoolConfig<ByteBuffer> config = new GenericObjectPoolConfig<>();
        config.setMaxTotal(poolSize);
        return new GenericObjectPool(new ByteBufferObjectFactory(bufferSize), config);
    });

    public ByteBufferPool(Integer poolSize, Integer bufferSize) {
        this.poolSize = poolSize;
        this.bufferSize = bufferSize;
    }

    public ByteBuffer borrow() throws Exception {
        return delegate.get().borrowObject();
    }

    public void recycle(ByteBuffer buffer) {
        delegate.get().returnObject(buffer);
    }
}

class ByteBufferObjectFactory implements PooledObjectFactory<ByteBuffer> {
    private Integer bufferSize;

    public ByteBufferObjectFactory(Integer bufferSize) {
        this.bufferSize = bufferSize;
    }

    @Override
    public void activateObject(PooledObject<ByteBuffer> p) throws Exception {
        p.getObject().clear();
    }

    @Override
    public void destroyObject(PooledObject<ByteBuffer> p) throws Exception {
        p.getObject().clear();
    }

    @Override
    public PooledObject<ByteBuffer> makeObject() throws Exception {
        return new DefaultPooledObject(ByteBuffer.allocate(bufferSize));
    }

    @Override
    public void passivateObject(PooledObject<ByteBuffer> p) throws Exception {
        p.getObject().clear();
    }

    @Override
    public boolean validateObject(PooledObject<ByteBuffer> p) {
        return p.getObject().capacity() == bufferSize && !p.getObject().isDirect();
    }
}