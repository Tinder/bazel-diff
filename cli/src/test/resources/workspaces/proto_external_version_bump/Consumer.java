package com.example.consumer;

import com.example.greeting.Greeting;

/** Trivial consumer of the generated Java proto from the remote proto_dep module. */
public final class Consumer {
  private Consumer() {}

  public static String describe() {
    return Greeting.getDefaultInstance().toString();
  }
}
